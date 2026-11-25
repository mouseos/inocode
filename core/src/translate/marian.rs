//! The Marian transformer models Firefox translates with (Bergamot):
//! reading the binary model and lexical shortlist files, and greedy
//! decoding on the CPU in f32 with the int8 weights scaled back as they
//! are used. Layout and arithmetic follow browsermt/marian-dev
//! (common/binary.cpp, tensors/cpu/integer_common.h, models/transformer.h,
//! rnn/cells.h, data/shortlist.cpp); docs/evidence/translation.md.

use std::collections::HashMap;

const FLOAT32: u64 = 0x404;
const INT8: u64 = 0x101;
const INTGEMM8: u64 = 0x4101;
const LAYER_NORM_EPSILON: f32 = 1e-6;
const SHORTLIST_MAGIC: u64 = 0xF11A_48D5_0134_17F5;

/// A tensor of a model file.
struct Item<'a> {
    kind: u64,
    shape: Vec<usize>,
    data: &'a [u8],
}

impl Item<'_> {
    fn elements(&self) -> usize {
        self.shape.iter().product()
    }
}

/// The tensors of a Marian binary model file by name.
fn items(data: &[u8]) -> Result<HashMap<String, Item<'_>>, String> {
    let mut at = 0usize;
    let u64_at = |at: &mut usize| -> Result<u64, String> {
        let bytes = data
            .get(*at..*at + 8)
            .ok_or("the model file is cut short")?;
        *at += 8;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    };
    let version = u64_at(&mut at)?;
    if version != 1 {
        return Err(format!("model file version {version} is not 1"));
    }
    let count = u64_at(&mut at)? as usize;
    if count > 100_000 {
        return Err("the model file has too many tensors".into());
    }
    let mut headers = Vec::with_capacity(count);
    for _ in 0..count {
        let name = u64_at(&mut at)? as usize;
        let kind = u64_at(&mut at)?;
        let shape = u64_at(&mut at)? as usize;
        let length = u64_at(&mut at)? as usize;
        headers.push((name, kind, shape, length));
    }
    let take = |at: &mut usize, length: usize| -> Result<&[u8], String> {
        let end = at
            .checked_add(length)
            .filter(|&end| end <= data.len())
            .ok_or("the model file is cut short")?;
        let bytes = &data[*at..end];
        *at = end;
        Ok(bytes)
    };
    let mut names = Vec::with_capacity(count);
    for &(length, ..) in &headers {
        let bytes = take(&mut at, length)?;
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        names.push(String::from_utf8_lossy(bytes).into_owned());
    }
    let mut shapes = Vec::with_capacity(count);
    for &(_, _, length, _) in &headers {
        let bytes = take(&mut at, length.checked_mul(4).ok_or("bad shape")?)?;
        let shape = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|dim| i32::from_le_bytes(*dim))
            .map(|dim| usize::try_from(dim).map_err(|_| "a negative dimension".to_string()))
            .collect::<Result<Vec<usize>, String>>()?;
        shapes.push(shape);
    }
    let padding = u64_at(&mut at)? as usize;
    take(&mut at, padding)?;
    let mut items = HashMap::with_capacity(count);
    for ((name, (_, kind, _, length)), shape) in names.into_iter().zip(headers).zip(shapes) {
        let data = take(&mut at, length)?;
        items.insert(name, Item { kind, shape, data });
    }
    Ok(items)
}

/// An int8 matrix kept one row per output, scaled back by `scale` as it
/// is used.
struct Rows {
    count: usize,
    width: usize,
    values: Vec<i8>,
    scale: f32,
}

impl Rows {
    fn row(&self, index: usize) -> &[i8] {
        &self.values[index * self.width..(index + 1) * self.width]
    }

    /// `x` times row `index`, scaled.
    fn dot(&self, index: usize, x: &[f32]) -> f32 {
        dot(x, self.row(index)) * self.scale
    }
}

fn dot(x: &[f32], w: &[i8]) -> f32 {
    const LANES: usize = 16;
    let mut sums = [0f32; LANES];
    let (x_chunks, x_rest) = x.as_chunks::<LANES>();
    let (w_chunks, w_rest) = w.as_chunks::<LANES>();
    for (x, w) in x_chunks.iter().zip(w_chunks) {
        for lane in 0..LANES {
            sums[lane] += x[lane] * w[lane] as f32;
        }
    }
    let mut sum: f32 = sums.iter().sum();
    for (x, w) in x_rest.iter().zip(w_rest) {
        sum += x * *w as f32;
    }
    sum
}

/// `y = x W + b`.
struct Linear {
    weights: Rows,
    bias: Option<Vec<f32>>,
}

impl Linear {
    fn inputs(&self) -> usize {
        self.weights.width
    }

    fn outputs(&self) -> usize {
        self.weights.count
    }

    /// Each row of `x` (`inputs` wide) times the matrix.
    fn apply(&self, x: &[f32]) -> Vec<f32> {
        let rows = x.len() / self.inputs();
        let mut out = vec![0f32; rows * self.outputs()];
        for (row, out) in x
            .chunks_exact(self.inputs())
            .zip(out.chunks_exact_mut(self.outputs()))
        {
            for (index, value) in out.iter_mut().enumerate() {
                *value = self.weights.dot(index, row)
                    + self.bias.as_ref().map_or(0.0, |bias| bias[index]);
            }
        }
        debug_assert_eq!(out.len(), rows * self.outputs());
        out
    }
}

struct LayerNorm {
    scale: Vec<f32>,
    bias: Vec<f32>,
}

impl LayerNorm {
    /// Normalises each row of `x` in place.
    fn apply(&self, x: &mut [f32]) {
        for row in x.chunks_exact_mut(self.scale.len()) {
            let mean = row.iter().sum::<f32>() / row.len() as f32;
            let variance =
                row.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / row.len() as f32;
            let inverse = 1.0 / (variance + LAYER_NORM_EPSILON).sqrt();
            for ((value, scale), bias) in row.iter_mut().zip(&self.scale).zip(&self.bias) {
                *value = (*value - mean) * inverse * scale + bias;
            }
        }
    }
}

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    output: Linear,
    norm: LayerNorm,
}

struct FeedForward {
    first: Linear,
    second: Linear,
    norm: LayerNorm,
}

#[derive(Clone, Copy)]
enum Activation {
    Relu,
    Swish,
}

/// The decoder's simpler simple recurrent unit (`SSRU` in rnn/cells.h).
struct Recurrent {
    input: Linear,
    forget: Linear,
    norm: LayerNorm,
}

struct DecoderLayer {
    recurrent: Recurrent,
    context: Attention,
    feed_forward: FeedForward,
}

pub(crate) struct Model {
    dim: usize,
    heads: usize,
    activation: Activation,
    source_embeddings: Rows,
    target_embeddings: Rows,
    output_bias: Vec<f32>,
    encoder: Vec<(Attention, FeedForward)>,
    decoder: Vec<DecoderLayer>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("dim", &self.dim)
            .field("encoder", &self.encoder.len())
            .field("decoder", &self.decoder.len())
            .finish()
    }
}

/// The `key: value` lines of the model's YAML configuration.
fn config(items: &HashMap<String, Item<'_>>) -> Result<HashMap<String, String>, String> {
    let item = items
        .get("special:model.yml")
        .filter(|item| item.kind == INT8)
        .ok_or("the model has no configuration")?;
    let text = String::from_utf8_lossy(item.data);
    Ok(text
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| !key.starts_with(' '))
        .map(|(key, value)| {
            let value = value.trim().trim_matches('"').to_string();
            (key.trim().to_string(), value)
        })
        .collect())
}

impl Model {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, String> {
        let items = items(data)?;
        let config = config(&items)?;
        let setting = |key: &str| config.get(key).map(String::as_str).unwrap_or("");
        let number = |key: &str| -> Result<usize, String> {
            setting(key)
                .parse()
                .map_err(|_| format!("the model setting {key} is not a number"))
        };
        let expect = |key: &str, wanted: &[&str]| -> Result<(), String> {
            let value = setting(key);
            if wanted.contains(&value) {
                Ok(())
            } else {
                Err(format!(
                    "the model setting {key}: {value:?} is not supported"
                ))
            }
        };
        expect("type", &["transformer"])?;
        expect("transformer-decoder-autoreg", &["rnn"])?;
        expect("dec-cell", &["ssru"])?;
        expect("transformer-postprocess", &["dan"])?;
        expect("transformer-preprocess", &[""])?;
        expect("transformer-postprocess-emb", &["d"])?;
        expect("transformer-postprocess-top", &[""])?;
        expect("transformer-ffn-depth", &["2"])?;
        expect("transformer-train-position-embeddings", &["false", ""])?;
        expect("layer-normalization", &["false", ""])?;
        expect("transformer-no-projection", &["false", ""])?;
        // The output layer is the target embeddings, or the one embedding
        // shared by both sides (`Wemb`).
        if setting("tied-embeddings") != "true" && setting("tied-embeddings-all") != "true" {
            return Err("models without tied embeddings are not supported".into());
        }
        let activation = match setting("transformer-ffn-activation") {
            "relu" => Activation::Relu,
            "swish" => Activation::Swish,
            other => return Err(format!("the activation {other:?} is not supported")),
        };
        let dim = number("dim-emb")?;
        let heads = number("transformer-heads")?;
        if heads == 0 || dim % heads != 0 {
            return Err("the heads do not divide the model width".into());
        }
        let get = |name: &str| items.get(name).ok_or(format!("the model has no {name}"));
        let floats = |name: &str, length: usize| -> Result<Vec<f32>, String> {
            let item = get(name)?;
            if item.kind != FLOAT32 || item.elements() != length || item.data.len() < length * 4 {
                return Err(format!("{name} is not {length} floats"));
            }
            Ok(item.data[..length * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes))
                .collect())
        };
        // intgemm8 tensors hold the int8 values then the f32 quantization
        // multiplier (integer_common.h). Embeddings are stored one row per
        // word; the other matrices transposed, one row per output.
        let rows = |name: &str, embedding: bool| -> Result<Rows, String> {
            let item = get(name)?;
            let [first, second] = item.shape[..] else {
                return Err(format!("{name} is not a matrix"));
            };
            let elements = first * second;
            if item.kind != INTGEMM8 || item.data.len() < elements + 4 {
                return Err(format!("{name} is not an int8 matrix"));
            }
            let multiplier =
                f32::from_le_bytes(item.data[elements..elements + 4].try_into().unwrap());
            if !multiplier.is_finite() || multiplier == 0.0 {
                return Err(format!("{name} has no quantization multiplier"));
            }
            let (count, width) = if embedding {
                (first, second)
            } else {
                (second, first)
            };
            Ok(Rows {
                count,
                width,
                values: item.data[..elements].iter().map(|&b| b as i8).collect(),
                scale: 1.0 / multiplier,
            })
        };
        let linear = |prefix: &str, weight: &str, bias: Option<&str>| -> Result<Linear, String> {
            let weights = rows(&format!("{prefix}_{weight}"), false)?;
            let bias = match bias {
                Some(bias) => Some(floats(&format!("{prefix}_{bias}"), weights.count)?),
                None => None,
            };
            Ok(Linear { weights, bias })
        };
        let norm = |prefix: &str| -> Result<LayerNorm, String> {
            Ok(LayerNorm {
                scale: floats(&format!("{prefix}_ln_scale"), dim)?,
                bias: floats(&format!("{prefix}_ln_bias"), dim)?,
            })
        };
        let attention = |prefix: &str| -> Result<Attention, String> {
            Ok(Attention {
                query: linear(prefix, "Wq", Some("bq"))?,
                key: linear(prefix, "Wk", Some("bk"))?,
                value: linear(prefix, "Wv", Some("bv"))?,
                output: linear(prefix, "Wo", Some("bo"))?,
                norm: norm(&format!("{prefix}_Wo"))?,
            })
        };
        let feed_forward = |prefix: &str| -> Result<FeedForward, String> {
            Ok(FeedForward {
                first: linear(prefix, "W1", Some("b1"))?,
                second: linear(prefix, "W2", Some("b2"))?,
                norm: norm(&format!("{prefix}_ffn"))?,
            })
        };
        let embeddings = |name: &str| -> Result<Rows, String> {
            if items.contains_key(name) {
                rows(name, true)
            } else {
                rows("Wemb", true)
            }
        };
        let source_embeddings = embeddings("encoder_Wemb")?;
        let target_embeddings = embeddings("decoder_Wemb")?;
        if source_embeddings.width != dim || target_embeddings.width != dim {
            return Err("the embeddings are not as wide as the model".into());
        }
        let output_bias = floats("decoder_ff_logit_out_b", target_embeddings.count)?;
        let encoder = (1..=number("enc-depth")?)
            .map(|layer| {
                Ok((
                    attention(&format!("encoder_l{layer}_self"))?,
                    feed_forward(&format!("encoder_l{layer}_ffn"))?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let decoder = (1..=number("dec-depth")?)
            .map(|layer| {
                let prefix = format!("decoder_l{layer}_rnn");
                Ok(DecoderLayer {
                    recurrent: Recurrent {
                        input: linear(&prefix, "W", None)?,
                        forget: linear(&prefix, "Wf", Some("bf"))?,
                        norm: norm(&format!("{prefix}_ffn"))?,
                    },
                    context: attention(&format!("decoder_l{layer}_context"))?,
                    feed_forward: feed_forward(&format!("decoder_l{layer}_ffn"))?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let model = Self {
            dim,
            heads,
            activation,
            source_embeddings,
            target_embeddings,
            output_bias,
            encoder,
            decoder,
        };
        model.check()?;
        Ok(model)
    }

    /// Checks that every matrix fits the width of the model.
    fn check(&self) -> Result<(), String> {
        let square = |linear: &Linear| linear.inputs() == self.dim && linear.outputs() == self.dim;
        let attention = |a: &Attention| {
            square(&a.query) && square(&a.key) && square(&a.value) && square(&a.output)
        };
        let feed_forward = |f: &FeedForward| {
            f.first.inputs() == self.dim
                && f.second.inputs() == f.first.outputs()
                && f.second.outputs() == self.dim
        };
        let fits = self
            .encoder
            .iter()
            .all(|(a, f)| attention(a) && feed_forward(f))
            && self.decoder.iter().all(|layer| {
                square(&layer.recurrent.input)
                    && square(&layer.recurrent.forget)
                    && attention(&layer.context)
                    && feed_forward(&layer.feed_forward)
            });
        if fits {
            Ok(())
        } else {
            Err("a matrix of the model does not fit its width".into())
        }
    }

    pub(crate) fn source_words(&self) -> usize {
        self.source_embeddings.count
    }

    pub(crate) fn target_words(&self) -> usize {
        self.target_embeddings.count
    }

    /// The bytes the weights take in memory.
    #[cfg(test)]
    pub(crate) fn size(&self) -> usize {
        let linear = |l: &Linear| l.weights.values.len() + 4 * l.bias.as_ref().map_or(0, Vec::len);
        let attention = |a: &Attention| {
            linear(&a.query) + linear(&a.key) + linear(&a.value) + linear(&a.output)
        };
        let feed_forward = |f: &FeedForward| linear(&f.first) + linear(&f.second);
        self.source_embeddings.values.len()
            + self.target_embeddings.values.len()
            + 4 * self.output_bias.len()
            + self
                .encoder
                .iter()
                .map(|(a, f)| attention(a) + feed_forward(f))
                .sum::<usize>()
            + self
                .decoder
                .iter()
                .map(|l| {
                    linear(&l.recurrent.input)
                        + linear(&l.recurrent.forget)
                        + attention(&l.context)
                        + feed_forward(&l.feed_forward)
                })
                .sum::<usize>()
    }

    /// The embedding of `word`, scaled by the square root of the width,
    /// plus the sinusoidal signal of `position` (transformer.h,
    /// `addPositionalEmbeddings`; node_initializers.cpp).
    fn embed(&self, embeddings: &Rows, word: Option<u32>, position: usize, out: &mut [f32]) {
        let scale = (self.dim as f32).sqrt();
        match word {
            Some(word) => {
                for (value, &weight) in out.iter_mut().zip(embeddings.row(word as usize)) {
                    *value = weight as f32 * embeddings.scale * scale;
                }
            }
            None => out.fill(0.0),
        }
        let timescales = self.dim / 2;
        let increment = (10000f32).ln() / (timescales as f32 - 1.0);
        for i in 0..timescales {
            let angle = position as f32 * (i as f32 * -increment).exp();
            out[i] += angle.sin();
            out[timescales + i] += angle.cos();
        }
    }

    fn feed_forward(&self, layer: &FeedForward, x: &mut [f32]) {
        let mut hidden = layer.first.apply(x);
        for value in &mut hidden {
            *value = match self.activation {
                Activation::Relu => value.max(0.0),
                Activation::Swish => *value / (1.0 + (-*value).exp()),
            };
        }
        let out = layer.second.apply(&hidden);
        add_and_normalize(x, &out, &layer.norm);
    }

    /// Attention of each row of `queries` over `keys` and `values` that
    /// went through the layer's key and value matrices already.
    fn attend(&self, layer: &Attention, x: &mut [f32], keys: &[f32], values: &[f32]) {
        let dim = self.dim;
        let depth = dim / self.heads;
        let queries = layer.query.apply(x);
        let length = keys.len() / dim;
        let scale = 1.0 / (depth as f32).sqrt();
        let mut joined = vec![0f32; queries.len()];
        let mut weights = vec![0f32; length];
        for (query, joined) in queries.chunks_exact(dim).zip(joined.chunks_exact_mut(dim)) {
            for head in 0..self.heads {
                let part = head * depth..(head + 1) * depth;
                let query = &query[part.clone()];
                for (weight, key) in weights.iter_mut().zip(keys.chunks_exact(dim)) {
                    *weight = query
                        .iter()
                        .zip(&key[part.clone()])
                        .map(|(q, k)| q * k)
                        .sum::<f32>()
                        * scale;
                }
                softmax(&mut weights);
                let joined = &mut joined[part.clone()];
                for (weight, value) in weights.iter().zip(values.chunks_exact(dim)) {
                    for (out, value) in joined.iter_mut().zip(&value[part.clone()]) {
                        *out += weight * value;
                    }
                }
            }
        }
        let out = layer.output.apply(&joined);
        add_and_normalize(x, &out, &layer.norm);
    }

    /// The encoder's output for the source `words`, one row each.
    fn encode(&self, words: &[u32]) -> Vec<f32> {
        let dim = self.dim;
        let mut x = vec![0f32; words.len() * dim];
        for (position, (word, row)) in words.iter().zip(x.chunks_exact_mut(dim)).enumerate() {
            self.embed(&self.source_embeddings, Some(*word), position, row);
        }
        for (attention, feed_forward) in &self.encoder {
            let keys = attention.key.apply(&x);
            let values = attention.value.apply(&x);
            self.attend(attention, &mut x, &keys, &values);
            self.feed_forward(feed_forward, &mut x);
        }
        x
    }

    /// The translation of the source `words` (ending with the end of
    /// sentence), choosing the likeliest word at each step among
    /// `candidates` (all words when `None`); at most `longest` words, and
    /// never `unknown`.
    pub(crate) fn translate(
        &self,
        words: &[u32],
        candidates: Option<&[u32]>,
        end: u32,
        unknown: u32,
        longest: usize,
    ) -> Vec<u32> {
        let dim = self.dim;
        let context = self.encode(words);
        let memory: Vec<(Vec<f32>, Vec<f32>)> = self
            .decoder
            .iter()
            .map(|layer| {
                (
                    layer.context.key.apply(&context),
                    layer.context.value.apply(&context),
                )
            })
            .collect();
        let all: Vec<u32>;
        let candidates = match candidates {
            Some(candidates) => candidates,
            None => {
                all = (0..self.target_words() as u32).collect();
                &all
            }
        };
        let mut cells = vec![vec![0f32; dim]; self.decoder.len()];
        let mut out = Vec::new();
        let mut previous = None;
        let mut x = vec![0f32; dim];
        while out.len() < longest {
            self.embed(&self.target_embeddings, previous, out.len(), &mut x);
            for ((layer, cell), (keys, values)) in self.decoder.iter().zip(&mut cells).zip(&memory)
            {
                let input = layer.recurrent.input.apply(&x);
                let forget = layer.recurrent.forget.apply(&x);
                let mut state = vec![0f32; dim];
                for i in 0..dim {
                    let gate = 1.0 / (1.0 + (-forget[i]).exp());
                    cell[i] = gate * cell[i] + (1.0 - gate) * input[i];
                    state[i] = cell[i].max(0.0);
                }
                add_and_normalize(&mut x, &state, &layer.recurrent.norm);
                self.attend(&layer.context, &mut x, keys, values);
                self.feed_forward(&layer.feed_forward, &mut x);
            }
            let mut best = (f32::NEG_INFINITY, end);
            for &word in candidates {
                if word == unknown || word as usize >= self.target_words() {
                    continue;
                }
                let score =
                    self.target_embeddings.dot(word as usize, &x) + self.output_bias[word as usize];
                if score > best.0 {
                    best = (score, word);
                }
            }
            if best.1 == end {
                break;
            }
            out.push(best.1);
            previous = Some(best.1);
        }
        out
    }
}

/// `x = norm(x + out)`, the transformer's "dan" after each block.
fn add_and_normalize(x: &mut [f32], out: &[f32], norm: &LayerNorm) {
    for (x, out) in x.iter_mut().zip(out) {
        *x += out;
    }
    norm.apply(x);
}

fn softmax(values: &mut [f32]) {
    let highest = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mut sum = 0.0;
    for value in values.iter_mut() {
        *value = (*value - highest).exp();
        sum += *value;
    }
    for value in values.iter_mut() {
        *value /= sum;
    }
}

/// Marian's binary lexical shortlist: for each source word, the target
/// words likely in its translation (data/shortlist.cpp,
/// `BinaryShortlistGenerator`).
#[derive(Debug)]
pub(crate) struct Shortlist {
    first: usize,
    offsets: Vec<u64>,
    words: Vec<u32>,
}

impl Shortlist {
    /// Reads the file: `magic, checksum, firstNum, bestNum,
    /// wordToOffsetSize, shortListsSize` as u64, the offsets as u64, then
    /// the target words as u32.
    pub(crate) fn parse(data: &[u8], target_words: usize) -> Result<Self, String> {
        let word = |index: usize| -> Option<u64> {
            data.get(index * 8..index * 8 + 8)
                .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
        };
        let header: Vec<u64> = (0..6)
            .map(word)
            .collect::<Option<_>>()
            .ok_or("the shortlist is cut short")?;
        if header[0] != SHORTLIST_MAGIC {
            return Err("the shortlist has the wrong magic number".into());
        }
        let offsets_count = usize::try_from(header[4]).map_err(|e| e.to_string())?;
        let words_count = usize::try_from(header[5]).map_err(|e| e.to_string())?;
        let expected = offsets_count
            .checked_mul(8)
            .and_then(|size| size.checked_add(words_count.checked_mul(4)?))
            .and_then(|size| size.checked_add(48))
            .ok_or("the shortlist sizes overflow")?;
        if data.len() != expected || offsets_count == 0 {
            return Err("the shortlist size does not match its header".into());
        }
        // util::hashMem over the u64 words after the checksum, with the
        // identity hash of libstdc++ and boost's hash_combine.
        let mut checksum = 0u64;
        for chunk in data[16..].as_chunks::<8>().0 {
            let value = u64::from_le_bytes(*chunk);
            checksum ^= value
                .wrapping_add(0x9e37_79b9)
                .wrapping_add(checksum << 6)
                .wrapping_add(checksum >> 2);
        }
        if checksum != header[1] {
            return Err("the shortlist checksum does not match".into());
        }
        let offsets: Vec<u64> = data[48..48 + offsets_count * 8]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|bytes| u64::from_le_bytes(*bytes))
            .collect();
        let words: Vec<u32> = data[48 + offsets_count * 8..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect();
        let ordered = offsets.windows(2).all(|pair| pair[0] <= pair[1]);
        if !ordered
            || offsets.last() != Some(&(words.len() as u64))
            || words.iter().any(|&word| word as usize >= target_words)
        {
            return Err("the shortlist does not fit the vocabulary".into());
        }
        Ok(Self {
            first: usize::try_from(header[2]).map_err(|e| e.to_string())?,
            offsets,
            words,
        })
    }

    /// The target words to choose from for `source` words, in order: the
    /// most frequent ones, those listed for each source word, and more of
    /// the frequent ones until there are a multiple of eight, as Marian
    /// does.
    pub(crate) fn candidates(&self, source: &[u32], target_words: usize) -> Vec<u32> {
        let mut chosen = vec![false; target_words];
        for flag in chosen.iter_mut().take(self.first) {
            *flag = true;
        }
        for &word in source {
            let word = word as usize;
            if word + 1 < self.offsets.len() {
                let range = self.offsets[word] as usize..self.offsets[word + 1] as usize;
                for &target in &self.words[range] {
                    chosen[target as usize] = true;
                }
            }
        }
        let mut count = chosen.iter().filter(|&&flag| flag).count();
        for flag in chosen.iter_mut().skip(self.first) {
            if count % 8 == 0 {
                break;
            }
            if !*flag {
                *flag = true;
                count += 1;
            }
        }
        chosen
            .iter()
            .enumerate()
            .filter(|(_, &flag)| flag)
            .map(|(index, _)| index as u32)
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A Marian binary model file of `tensors` (name, type, shape, data).
    pub(crate) fn file(tensors: &[(&str, u64, Vec<i32>, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        for (name, kind, shape, data) in tensors {
            for value in [
                name.len() as u64 + 1,
                *kind,
                shape.len() as u64,
                data.len() as u64,
            ] {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        for (name, ..) in tensors {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        for (_, _, shape, _) in tensors {
            for dim in shape {
                out.extend_from_slice(&dim.to_le_bytes());
            }
        }
        let next = ((out.len() + 8) / 256 + 1) * 256;
        let padding = next - out.len() - 8;
        out.extend_from_slice(&(padding as u64).to_le_bytes());
        out.resize(out.len() + padding, 0);
        for (.., data) in tensors {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn model_files_are_read_by_name() {
        let mut matrix: Vec<u8> = [1i8, -2, 3, 4, 5, -6].iter().map(|&v| v as u8).collect();
        matrix.extend_from_slice(&2.0f32.to_le_bytes());
        matrix.resize(6 + 256, 0);
        let bias: Vec<u8> = [0.5f32, 1.5, -1.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let data = file(&[
            ("layer_W", INTGEMM8, vec![2, 3], matrix),
            ("layer_b", FLOAT32, vec![1, 3], bias),
            ("special:model.yml", INT8, vec![9], b"type: x\n\0".to_vec()),
        ]);
        let read = items(&data).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read["layer_W"].shape, [2, 3]);
        assert_eq!(read["layer_W"].data.len(), 262);
        assert_eq!(config(&read).unwrap()["type"], "x");
        assert!(items(&data[..data.len() - 1]).is_err());
        let mut wrong = data.clone();
        wrong[0] = 2;
        assert!(items(&wrong).is_err());
        assert!(Model::parse(&data).unwrap_err().contains("type"));

        // The [in=2, out=3] matrix is stored transposed: row j is output j.
        let item = &read["layer_W"];
        let weights = Rows {
            count: 3,
            width: 2,
            values: item.data[..6].iter().map(|&b| b as i8).collect(),
            scale: 0.5,
        };
        let linear = Linear {
            weights,
            bias: Some(vec![0.5, 1.5, -1.0]),
        };
        // [1, 1] -> rows (1,-2), (3,4), (5,-6) halved, plus the bias.
        assert_eq!(linear.apply(&[1.0, 1.0]), [0.0, 5.0, -1.5]);
    }

    /// A shortlist file of `first` frequent words and, per source word,
    /// its target words.
    pub(crate) fn shortlist_file(first: u64, lists: &[&[u32]]) -> Vec<u8> {
        let mut offsets = vec![0u64];
        let mut words = Vec::new();
        for list in lists {
            words.extend_from_slice(list);
            offsets.push(words.len() as u64);
        }
        let mut body = Vec::new();
        for value in [first, 50, offsets.len() as u64, words.len() as u64] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        for offset in &offsets {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        for word in &words {
            body.extend_from_slice(&word.to_le_bytes());
        }
        let mut checksum = 0u64;
        for chunk in body.as_chunks::<8>().0 {
            let value = u64::from_le_bytes(*chunk);
            checksum ^= value
                .wrapping_add(0x9e37_79b9)
                .wrapping_add(checksum << 6)
                .wrapping_add(checksum >> 2);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&SHORTLIST_MAGIC.to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn shortlists_give_frequent_and_listed_words() {
        let data = shortlist_file(2, &[&[], &[5, 9], &[20, 21]]);
        let shortlist = Shortlist::parse(&data, 30).unwrap();
        // 0, 1 frequent; 5, 9 for source word 1; then 2..=5 more to make 8.
        assert_eq!(shortlist.candidates(&[1, 1], 30), [0, 1, 2, 3, 4, 5, 6, 9]);
        assert_eq!(shortlist.candidates(&[2, 1], 30).len() % 8, 0);
        assert!(Shortlist::parse(&data, 21).is_err());
        let mut broken = data.clone();
        let last = broken.len() - 5;
        broken[last] ^= 1;
        assert!(Shortlist::parse(&broken, 30).is_err());
    }
}
