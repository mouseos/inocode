//! A reader and interpreter for TensorFlow Lite model files, for the
//! translation models Chrome downloads: the flatbuffer layout of TFLite's
//! `schema.fbs` and, for each builtin operator those graphs use, the
//! arithmetic of TFLite's reference kernels (`tensorflow/lite/kernels`).
//! A file with any other operator or tensor type is refused when it is read.

use std::collections::HashMap;
use std::sync::Arc;

/// The values of a tensor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Data {
    F32(Vec<f32>),
    I32(Vec<i32>),
    I8(Vec<i8>),
    Bool(Vec<bool>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    F32,
    I32,
    I8,
    Bool,
}

impl Data {
    fn kind(&self) -> Kind {
        match self {
            Self::F32(_) => Kind::F32,
            Self::I32(_) => Kind::I32,
            Self::I8(_) => Kind::I8,
            Self::Bool(_) => Kind::Bool,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F32(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::Bool(v) => v.len(),
        }
    }

    /// The elements at `indices`, in that order.
    fn pick(&self, indices: &[usize]) -> Data {
        fn take<T: Copy>(values: &[T], indices: &[usize]) -> Vec<T> {
            indices.iter().map(|&index| values[index]).collect()
        }
        match self {
            Self::F32(v) => Self::F32(take(v, indices)),
            Self::I32(v) => Self::I32(take(v, indices)),
            Self::I8(v) => Self::I8(take(v, indices)),
            Self::Bool(v) => Self::Bool(take(v, indices)),
        }
    }

    /// The parts one after another; they must all be of one type.
    fn join(parts: &[&Data]) -> Result<Data, String> {
        let first = parts.first().ok_or("nothing to join")?;
        let mut out = match first {
            Self::F32(_) => Self::F32(Vec::new()),
            Self::I32(_) => Self::I32(Vec::new()),
            Self::I8(_) => Self::I8(Vec::new()),
            Self::Bool(_) => Self::Bool(Vec::new()),
        };
        for part in parts {
            match (&mut out, part) {
                (Self::F32(out), Self::F32(v)) => out.extend_from_slice(v),
                (Self::I32(out), Self::I32(v)) => out.extend_from_slice(v),
                (Self::I8(out), Self::I8(v)) => out.extend_from_slice(v),
                (Self::Bool(out), Self::Bool(v)) => out.extend_from_slice(v),
                _ => return Err("the inputs differ in type".into()),
            }
        }
        Ok(out)
    }
}

/// A tensor's shape and values; scalars have an empty shape.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tensor {
    pub(crate) shape: Vec<usize>,
    pub(crate) data: Data,
}

impl Tensor {
    pub(crate) fn f32(shape: Vec<usize>, values: Vec<f32>) -> Self {
        Self {
            shape,
            data: Data::F32(values),
        }
    }

    pub(crate) fn i32(shape: Vec<usize>, values: Vec<i32>) -> Self {
        Self {
            shape,
            data: Data::I32(values),
        }
    }

    fn new(shape: Vec<usize>, data: Data) -> Result<Self, String> {
        if shape.iter().product::<usize>() != data.len() {
            return Err(format!(
                "{} values do not fill the shape {shape:?}",
                data.len()
            ));
        }
        Ok(Self { shape, data })
    }

    pub(crate) fn floats(&self) -> Result<&[f32], String> {
        match &self.data {
            Data::F32(v) => Ok(v),
            other => Err(format!("expected float32 values, not {:?}", other.kind())),
        }
    }

    fn ints(&self) -> Result<&[i32], String> {
        match &self.data {
            Data::I32(v) => Ok(v),
            other => Err(format!("expected int32 values, not {:?}", other.kind())),
        }
    }

    fn bools(&self) -> Result<&[bool], String> {
        match &self.data {
            Data::Bool(v) => Ok(v),
            other => Err(format!("expected bool values, not {:?}", other.kind())),
        }
    }

    /// The single integer of a scalar or one-element tensor.
    fn scalar_int(&self) -> Result<i32, String> {
        match self.ints()? {
            [value] => Ok(*value),
            _ => Err("expected one integer".into()),
        }
    }
}

/// How a quantized tensor maps to real numbers:
/// `real = (value - zero point) * scale`, per channel along `dimension`
/// when there are several scales.
#[derive(Debug, Clone, PartialEq)]
struct Quantization {
    scales: Vec<f32>,
    zero_points: Vec<i64>,
    dimension: usize,
}

impl Quantization {
    fn channel(&self, channel: usize) -> (f32, i64) {
        let at = if self.scales.len() == 1 { 0 } else { channel };
        (
            self.scales[at],
            self.zero_points.get(at).copied().unwrap_or(0),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Activation {
    None,
    Relu,
    ReluN1To1,
    Relu6,
    Tanh,
}

impl Activation {
    fn from_code(code: u8) -> Result<Self, String> {
        Ok(match code {
            0 => Self::None,
            1 => Self::Relu,
            2 => Self::ReluN1To1,
            3 => Self::Relu6,
            4 => Self::Tanh,
            other => return Err(format!("unsupported fused activation {other}")),
        })
    }

    fn apply(self, values: &mut [f32]) {
        let f: fn(f32) -> f32 = match self {
            Self::None => return,
            Self::Relu => |x| x.max(0.0),
            Self::ReluN1To1 => |x| x.clamp(-1.0, 1.0),
            Self::Relu6 => |x| x.clamp(0.0, 6.0),
            Self::Tanh => f32::tanh,
        };
        for value in values {
            *value = f(*value);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Add(Activation),
    Sub(Activation),
    Mul(Activation),
    SquaredDifference,
    FloorDiv,
    Greater,
    Rsqrt,
    Sin,
    Cos,
    Logistic,
    Tanh,
    Reshape(Option<Vec<i32>>),
    Shape,
    StridedSlice {
        begin_mask: i32,
        end_mask: i32,
        shrink_axis_mask: i32,
        offset: bool,
    },
    Pack(i32),
    Range,
    Cast,
    ExpandDims,
    Concatenation(i32, Activation),
    Pad,
    Transpose,
    Tile,
    Gather(i32),
    Dequantize,
    Mean(bool),
    Fill,
    FullyConnected {
        activation: Activation,
        keep_num_dims: bool,
        asymmetric: bool,
    },
    BatchMatMul {
        adj_x: bool,
        adj_y: bool,
    },
    Select,
    SelectV2,
    Softmax(f32),
    LogSoftmax,
    Split,
}

#[derive(Debug, Clone)]
struct Operator {
    op: Op,
    /// `None` for an optional input that is left out.
    inputs: Vec<Option<usize>>,
    outputs: Vec<usize>,
}

/// A model's main subgraph, ready to run.
pub(crate) struct Graph {
    names: Vec<String>,
    kinds: Vec<Kind>,
    /// The shape each tensor is declared with, -1 where it varies.
    shapes: Vec<Vec<i32>>,
    quantization: Vec<Option<Quantization>>,
    constants: Vec<Option<Arc<Tensor>>>,
    operators: Vec<Operator>,
    inputs: Vec<usize>,
    outputs: Vec<usize>,
    /// The last operator that reads each tensor, so its value can go.
    last_use: Vec<Option<usize>>,
    /// The sum of each row of the int8 weights of fully connected
    /// operators, for inputs quantized with an offset.
    row_sums: HashMap<usize, Vec<i32>>,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("tensors", &self.names.len())
            .field("operators", &self.operators.len())
            .finish()
    }
}

impl Graph {
    /// Reads a `.tflite` file.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, String> {
        let model = Table::root(data)?;
        let codes = model
            .tables(1)?
            .iter()
            .map(|code| {
                let deprecated = code.u8(0, 0)? as i32;
                let builtin = code.i32(3, 0)?;
                Ok(deprecated.max(builtin))
            })
            .collect::<Result<Vec<i32>, String>>()?;
        let buffers = model.tables(4)?;
        let subgraphs = model.tables(2)?;
        let graph = subgraphs.first().ok_or("the model has no subgraph")?;
        let tensor_tables = graph.tables(0)?;
        let count = tensor_tables.len();
        let mut names = Vec::with_capacity(count);
        let mut kinds = Vec::with_capacity(count);
        let mut shapes = Vec::with_capacity(count);
        let mut quantization = Vec::with_capacity(count);
        let mut constants = Vec::with_capacity(count);
        for tensor in &tensor_tables {
            let name = tensor.string(3)?.unwrap_or_default();
            let kind = match tensor.u8(1, 0)? {
                0 => Kind::F32,
                2 => Kind::I32,
                6 => Kind::Bool,
                9 => Kind::I8,
                other => return Err(format!("tensor {name:?} has unsupported type {other}")),
            };
            let shape = tensor.ints(0)?.unwrap_or_default();
            let signature = tensor.ints(7)?.unwrap_or_else(|| shape.clone());
            let quantized = match tensor.table(4)? {
                Some(table) => {
                    let scales = table.floats(2)?.unwrap_or_default();
                    let zero_points = table.longs(3)?.unwrap_or_default();
                    let dimension = table.i32(6, 0)?;
                    if scales.is_empty() {
                        None
                    } else {
                        if dimension < 0 {
                            return Err(format!(
                                "tensor {name:?} has a negative quantized dimension"
                            ));
                        }
                        Some(Quantization {
                            scales,
                            zero_points,
                            dimension: dimension as usize,
                        })
                    }
                }
                None => None,
            };
            let buffer = tensor.u32(2, 0)? as usize;
            let bytes = match buffer {
                0 => None,
                index => buffers
                    .get(index)
                    .ok_or_else(|| format!("tensor {name:?} names a missing buffer"))?
                    .bytes(0)?
                    .filter(|bytes| !bytes.is_empty()),
            };
            let constant = match bytes {
                Some(bytes) => {
                    let dims = shape
                        .iter()
                        .map(|&d| {
                            usize::try_from(d)
                                .map_err(|_| format!("tensor {name:?} has a negative dimension"))
                        })
                        .collect::<Result<Vec<usize>, String>>()?;
                    let data = decode(kind, bytes)?;
                    Some(Arc::new(
                        Tensor::new(dims, data)
                            .map_err(|error| format!("tensor {name:?}: {error}"))?,
                    ))
                }
                None => None,
            };
            names.push(name);
            kinds.push(kind);
            shapes.push(signature);
            quantization.push(quantized);
            constants.push(constant);
        }
        let index = |value: i32| -> Result<usize, String> {
            usize::try_from(value)
                .ok()
                .filter(|&index| index < count)
                .ok_or_else(|| format!("tensor index {value} is out of range"))
        };
        let inputs = graph
            .ints(1)?
            .unwrap_or_default()
            .into_iter()
            .map(index)
            .collect::<Result<Vec<usize>, String>>()?;
        let outputs = graph
            .ints(2)?
            .unwrap_or_default()
            .into_iter()
            .map(index)
            .collect::<Result<Vec<usize>, String>>()?;
        let mut operators = Vec::new();
        for (at, table) in graph.tables(3)?.iter().enumerate() {
            let code = *codes
                .get(table.u32(0, 0)? as usize)
                .ok_or_else(|| format!("operator {at} names a missing operator code"))?;
            let options = table.table(4)?;
            let op = op(code, options).map_err(|error| format!("operator {at}: {error}"))?;
            let operator_inputs = table
                .ints(1)?
                .unwrap_or_default()
                .into_iter()
                .map(|value| {
                    if value < 0 {
                        Ok(None)
                    } else {
                        index(value).map(Some)
                    }
                })
                .collect::<Result<Vec<Option<usize>>, String>>()?;
            let operator_outputs = table
                .ints(2)?
                .unwrap_or_default()
                .into_iter()
                .map(index)
                .collect::<Result<Vec<usize>, String>>()?;
            operators.push(Operator {
                op,
                inputs: operator_inputs,
                outputs: operator_outputs,
            });
        }
        let mut last_use = vec![None; count];
        for (at, operator) in operators.iter().enumerate() {
            for input in operator.inputs.iter().flatten() {
                last_use[*input] = Some(at);
            }
        }
        let mut row_sums = HashMap::new();
        for operator in &operators {
            if let (Op::FullyConnected { .. }, Some(Some(weights))) =
                (&operator.op, operator.inputs.get(1))
            {
                if let Some(tensor) = &constants[*weights] {
                    if let (Data::I8(values), [_, columns]) =
                        (&tensor.data, tensor.shape.as_slice())
                    {
                        if *columns > 0 {
                            let sums = values
                                .chunks(*columns)
                                .map(|row| row.iter().map(|&v| v as i32).sum())
                                .collect();
                            row_sums.insert(*weights, sums);
                        }
                    }
                }
            }
        }
        Ok(Self {
            names,
            kinds,
            shapes,
            quantization,
            constants,
            operators,
            inputs,
            outputs,
            last_use,
            row_sums,
        })
    }

    /// The position among the inputs of the one whose name contains `name`.
    pub(crate) fn input(&self, name: &str) -> Result<usize, String> {
        find(&self.inputs, &self.names, name)
            .ok_or_else(|| format!("the graph has no input {name:?}"))
    }

    /// The position among the outputs of the one whose name contains `name`.
    pub(crate) fn output(&self, name: &str) -> Result<usize, String> {
        find(&self.outputs, &self.names, name)
            .ok_or_else(|| format!("the graph has no output {name:?}"))
    }

    pub(crate) fn input_names(&self) -> Vec<&str> {
        self.inputs
            .iter()
            .map(|&index| self.names[index].as_str())
            .collect()
    }

    pub(crate) fn output_names(&self) -> Vec<&str> {
        self.outputs
            .iter()
            .map(|&index| self.names[index].as_str())
            .collect()
    }

    /// The declared shape of input `position`, -1 where it varies.
    pub(crate) fn input_shape(&self, position: usize) -> &[i32] {
        &self.shapes[self.inputs[position]]
    }

    /// Runs the graph on `inputs`, given in the graph's input order, and
    /// gives back its outputs in their order.
    pub(crate) fn run(&self, inputs: Vec<Tensor>) -> Result<Vec<Tensor>, String> {
        if inputs.len() != self.inputs.len() {
            return Err(format!(
                "the graph takes {} inputs, not {}",
                self.inputs.len(),
                inputs.len()
            ));
        }
        let mut values = self.constants.clone();
        for (&index, tensor) in self.inputs.iter().zip(inputs) {
            if tensor.data.kind() != self.kinds[index] {
                return Err(format!("input {:?} has the wrong type", self.names[index]));
            }
            values[index] = Some(Arc::new(tensor));
        }
        for (at, operator) in self.operators.iter().enumerate() {
            let produced = {
                let args = operator
                    .inputs
                    .iter()
                    .map(|input| match input {
                        None => Ok(None),
                        Some(index) => values[*index]
                            .as_deref()
                            .map(Some)
                            .ok_or_else(|| format!("{:?} has no value yet", self.names[*index])),
                    })
                    .collect::<Result<Vec<Option<&Tensor>>, String>>()?;
                self.apply(operator, &args)
                    .map_err(|error| format!("operator {at} ({:?}): {error}", operator.op))?
            };
            if produced.len() != operator.outputs.len() {
                return Err(format!("operator {at} gave the wrong number of outputs"));
            }
            for (&index, tensor) in operator.outputs.iter().zip(produced) {
                if tensor.data.kind() != self.kinds[index] {
                    return Err(format!(
                        "operator {at} gave {:?} for {:?}, declared {:?}",
                        tensor.data.kind(),
                        self.names[index],
                        self.kinds[index]
                    ));
                }
                values[index] = Some(Arc::new(tensor));
            }
            for index in operator.inputs.iter().flatten() {
                if self.last_use[*index] == Some(at)
                    && self.constants[*index].is_none()
                    && !self.outputs.contains(index)
                {
                    values[*index] = None;
                }
            }
        }
        self.outputs
            .iter()
            .map(|&index| {
                values[index]
                    .as_ref()
                    .map(|tensor| (**tensor).clone())
                    .ok_or_else(|| format!("output {:?} was never computed", self.names[index]))
            })
            .collect()
    }

    fn apply(&self, operator: &Operator, args: &[Option<&Tensor>]) -> Result<Vec<Tensor>, String> {
        let arg = |position: usize| -> Result<&Tensor, String> {
            args.get(position)
                .copied()
                .flatten()
                .ok_or_else(|| format!("input {position} is missing"))
        };
        let output_kind = |position: usize| -> Result<Kind, String> {
            operator
                .outputs
                .get(position)
                .map(|&index| self.kinds[index])
                .ok_or_else(|| "an output is missing".to_string())
        };
        let one = |tensor: Tensor| Ok(vec![tensor]);
        match &operator.op {
            Op::Add(activation) => one(arithmetic(
                arg(0)?,
                arg(1)?,
                *activation,
                |a, b| a + b,
                |a, b| Ok(a.wrapping_add(b)),
            )?),
            Op::Sub(activation) => one(arithmetic(
                arg(0)?,
                arg(1)?,
                *activation,
                |a, b| a - b,
                |a, b| Ok(a.wrapping_sub(b)),
            )?),
            Op::Mul(activation) => one(arithmetic(
                arg(0)?,
                arg(1)?,
                *activation,
                |a, b| a * b,
                |a, b| Ok(a.wrapping_mul(b)),
            )?),
            Op::SquaredDifference => one(arithmetic(
                arg(0)?,
                arg(1)?,
                Activation::None,
                |a, b| (a - b) * (a - b),
                |a, b| Ok(a.wrapping_sub(b).wrapping_mul(a.wrapping_sub(b))),
            )?),
            Op::FloorDiv => one(arithmetic(
                arg(0)?,
                arg(1)?,
                Activation::None,
                |a, b| (a / b).floor(),
                |a, b| {
                    if b == 0 {
                        Err("division by zero".to_string())
                    } else {
                        Ok((a as f64 / b as f64).floor() as i32)
                    }
                },
            )?),
            Op::Greater => one(greater(arg(0)?, arg(1)?)?),
            Op::Rsqrt => one(unary(arg(0)?, |x| 1.0 / x.sqrt())?),
            Op::Sin => one(unary(arg(0)?, f32::sin)?),
            Op::Cos => one(unary(arg(0)?, f32::cos)?),
            Op::Logistic => one(unary(arg(0)?, |x| 1.0 / (1.0 + (-x).exp()))?),
            Op::Tanh => one(unary(arg(0)?, f32::tanh)?),
            Op::Reshape(new_shape) => {
                let shape = match args.get(1).copied().flatten() {
                    Some(shape) => shape.ints()?.to_vec(),
                    None => new_shape.clone().ok_or("no shape to reshape to")?,
                };
                one(reshape(arg(0)?, &shape)?)
            }
            Op::Shape => {
                let input = arg(0)?;
                let dims = input
                    .shape
                    .iter()
                    .map(|&d| i32::try_from(d).map_err(|_| "a dimension is too large".to_string()))
                    .collect::<Result<Vec<i32>, String>>()?;
                one(Tensor::i32(vec![dims.len()], dims))
            }
            Op::StridedSlice {
                begin_mask,
                end_mask,
                shrink_axis_mask,
                offset,
            } => one(strided_slice(
                arg(0)?,
                arg(1)?.ints()?,
                arg(2)?.ints()?,
                arg(3)?.ints()?,
                *begin_mask,
                *end_mask,
                *shrink_axis_mask,
                *offset,
            )?),
            Op::Pack(axis) => {
                let parts = args
                    .iter()
                    .map(|part| part.ok_or("an input is missing"))
                    .collect::<Result<Vec<&Tensor>, &str>>()?;
                one(pack(&parts, *axis)?)
            }
            Op::Range => one(range(arg(0)?, arg(1)?, arg(2)?)?),
            Op::Cast => one(cast(arg(0)?, output_kind(0)?)?),
            Op::ExpandDims => {
                let input = arg(0)?;
                let rank = input.shape.len() as i32;
                let mut axis = arg(1)?.scalar_int()?;
                if axis < 0 {
                    axis += rank + 1;
                }
                if !(0..=rank).contains(&axis) {
                    return Err(format!("axis {axis} is out of range"));
                }
                let mut shape = input.shape.clone();
                shape.insert(axis as usize, 1);
                one(Tensor::new(shape, input.data.clone())?)
            }
            Op::Concatenation(axis, activation) => {
                let parts = args
                    .iter()
                    .map(|part| part.ok_or("an input is missing"))
                    .collect::<Result<Vec<&Tensor>, &str>>()?;
                let mut out = concatenate(&parts, *axis)?;
                if let Data::F32(values) = &mut out.data {
                    activation.apply(values);
                }
                one(out)
            }
            Op::Pad => {
                let input = arg(0)?;
                let zero = operator.inputs[0]
                    .and_then(|index| self.quantization[index].as_ref())
                    .map_or(0, |quantization| quantization.channel(0).1);
                one(pad(input, arg(1)?.ints()?, zero)?)
            }
            Op::Transpose => one(transpose(arg(0)?, arg(1)?.ints()?)?),
            Op::Tile => one(tile(arg(0)?, arg(1)?.ints()?)?),
            Op::Gather(axis) => one(gather(arg(0)?, arg(1)?, *axis)?),
            Op::Dequantize => {
                let input = arg(0)?;
                let quantization = operator.inputs[0]
                    .and_then(|index| self.quantization[index].as_ref())
                    .ok_or("the input is not quantized")?;
                one(dequantize(input, quantization)?)
            }
            Op::Mean(keep_dims) => one(mean(arg(0)?, arg(1)?.ints()?, *keep_dims)?),
            Op::Fill => {
                let dims = arg(0)?
                    .ints()?
                    .iter()
                    .map(|&d| usize::try_from(d).map_err(|_| format!("dimension {d} is negative")))
                    .collect::<Result<Vec<usize>, String>>()?;
                let value = arg(1)?;
                if value.data.len() != 1 {
                    return Err("the fill value is not a scalar".into());
                }
                let total = dims.iter().product::<usize>();
                one(Tensor::new(dims, value.data.pick(&vec![0; total]))?)
            }
            Op::FullyConnected {
                activation,
                keep_num_dims,
                asymmetric,
            } => {
                let weights_index = operator
                    .inputs
                    .get(1)
                    .copied()
                    .flatten()
                    .ok_or("the weights are missing")?;
                let weights = arg(1)?;
                let bias = args.get(2).copied().flatten();
                let quantized = match weights.data {
                    Data::I8(_) => Some((
                        self.quantization[weights_index]
                            .as_ref()
                            .ok_or("the int8 weights are not quantized")?,
                        self.row_sums.get(&weights_index).map(Vec::as_slice),
                    )),
                    _ => None,
                };
                one(fully_connected(
                    arg(0)?,
                    weights,
                    bias,
                    quantized,
                    *activation,
                    *keep_num_dims,
                    *asymmetric,
                )?)
            }
            Op::BatchMatMul { adj_x, adj_y } => {
                one(batch_matmul(arg(0)?, arg(1)?, *adj_x, *adj_y)?)
            }
            Op::Select => one(select(arg(0)?, arg(1)?, arg(2)?)?),
            Op::SelectV2 => one(select_v2(arg(0)?, arg(1)?, arg(2)?)?),
            Op::Softmax(beta) => one(softmax(arg(0)?, *beta, false)?),
            Op::LogSoftmax => one(softmax(arg(0)?, 1.0, true)?),
            Op::Split => split(arg(1)?, arg(0)?.scalar_int()?, operator.outputs.len()),
        }
    }
}

fn find(positions: &[usize], names: &[String], name: &str) -> Option<usize> {
    positions
        .iter()
        .position(|&index| names[index].contains(name))
}

/// The values of a constant buffer, little-endian.
fn decode(kind: Kind, bytes: &[u8]) -> Result<Data, String> {
    let words = |size: usize| -> Result<(), String> {
        if bytes.len().is_multiple_of(size) {
            Ok(())
        } else {
            Err("a buffer does not fit its type".into())
        }
    };
    Ok(match kind {
        Kind::F32 => {
            words(4)?;
            Data::F32(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|word| f32::from_le_bytes(*word))
                    .collect(),
            )
        }
        Kind::I32 => {
            words(4)?;
            Data::I32(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|word| i32::from_le_bytes(*word))
                    .collect(),
            )
        }
        Kind::I8 => Data::I8(bytes.iter().map(|&byte| byte as i8).collect()),
        Kind::Bool => Data::Bool(bytes.iter().map(|&byte| byte != 0).collect()),
    })
}

/// The operator for builtin `code` with its `options` table
/// (`BuiltinOperator` and the `*Options` tables of schema.fbs).
fn op(code: i32, options: Option<Table>) -> Result<Op, String> {
    let int =
        |field: usize, default: i32| options.map_or(Ok(default), |table| table.i32(field, default));
    let flag = |field: usize| {
        options.map_or(Ok(false), |table| {
            table.u8(field, 0).map(|value| value != 0)
        })
    };
    let activation = |field: usize| {
        options
            .map_or(Ok(0), |table| table.u8(field, 0))
            .and_then(Activation::from_code)
    };
    Ok(match code {
        0 => Op::Add(activation(0)?),
        2 => Op::Concatenation(int(0, 0)?, activation(1)?),
        6 => Op::Dequantize,
        9 => {
            if options.map_or(Ok(0), |table| table.u8(1, 0))? != 0 {
                return Err("shuffled fully connected weights are not supported".into());
            }
            Op::FullyConnected {
                activation: activation(0)?,
                keep_num_dims: flag(2)?,
                asymmetric: flag(3)?,
            }
        }
        14 => Op::Logistic,
        18 => Op::Mul(activation(0)?),
        22 => Op::Reshape(match options {
            Some(table) => table.ints(0)?,
            None => None,
        }),
        25 => Op::Softmax(options.map_or(Ok(1.0), |table| table.f32(0, 0.0))?),
        28 => Op::Tanh,
        34 => Op::Pad,
        36 => {
            if int(1, 0)? != 0 {
                return Err("gather with batch dimensions is not supported".into());
            }
            Op::Gather(int(0, 0)?)
        }
        39 => Op::Transpose,
        40 => Op::Mean(flag(0)?),
        41 => Op::Sub(activation(0)?),
        45 => {
            if int(2, 0)? != 0 || int(3, 0)? != 0 {
                return Err("strided slice with ellipsis or new axes is not supported".into());
            }
            Op::StridedSlice {
                begin_mask: int(0, 0)?,
                end_mask: int(1, 0)?,
                shrink_axis_mask: int(4, 0)?,
                offset: flag(5)?,
            }
        }
        49 => Op::Split,
        50 => Op::LogSoftmax,
        53 => Op::Cast,
        61 => Op::Greater,
        64 => Op::Select,
        66 => Op::Sin,
        69 => Op::Tile,
        70 => Op::ExpandDims,
        76 => Op::Rsqrt,
        77 => {
            if options.map_or(Ok(2), |table| table.u8(0, 2))? != 2 {
                return Err("shape is only supported as int32".into());
            }
            Op::Shape
        }
        83 => Op::Pack(int(1, 0)?),
        90 => Op::FloorDiv,
        94 => Op::Fill,
        96 => Op::Range,
        99 => Op::SquaredDifference,
        108 => Op::Cos,
        123 => Op::SelectV2,
        126 => {
            if flag(2)? {
                return Err("quantized batch matmul is not supported".into());
            }
            Op::BatchMatMul {
                adj_x: flag(0)?,
                adj_y: flag(1)?,
            }
        }
        other => return Err(format!("builtin operator {other} is not supported")),
    })
}

/// The shape `shapes` broadcast to, numpy style.
fn broadcast_shape(shapes: &[&[usize]]) -> Result<Vec<usize>, String> {
    let rank = shapes.iter().map(|shape| shape.len()).max().unwrap_or(0);
    let mut out = vec![1; rank];
    for shape in shapes {
        for (from_end, &dim) in shape.iter().rev().enumerate() {
            let slot = &mut out[rank - 1 - from_end];
            if *slot == 1 {
                *slot = dim;
            } else if dim != 1 && dim != *slot {
                return Err(format!("shapes {shapes:?} do not broadcast"));
            }
        }
    }
    Ok(out)
}

/// For each element of a tensor of shape `out`, the index of the element
/// of a tensor of `shape` that broadcasts to it.
fn broadcast_indices(shape: &[usize], out: &[usize]) -> Vec<usize> {
    let total: usize = out.iter().product();
    if shape == out {
        return (0..total).collect();
    }
    let rank = out.len();
    let lead = rank - shape.len();
    let mut strides = vec![0; rank];
    let mut stride = 1;
    for axis in (0..shape.len()).rev() {
        strides[lead + axis] = if shape[axis] == 1 { 0 } else { stride };
        stride *= shape[axis];
    }
    odometer(out, &strides, 0)
}

/// For each multi-index over `dims` in row-major order, `base` plus the
/// sum of index times stride.
fn odometer(dims: &[usize], strides: &[usize], base: usize) -> Vec<usize> {
    let total: usize = dims.iter().product();
    let mut out = Vec::with_capacity(total);
    let mut index = vec![0usize; dims.len()];
    let mut at = base;
    for _ in 0..total {
        out.push(at);
        for axis in (0..dims.len()).rev() {
            index[axis] += 1;
            at += strides[axis];
            if index[axis] < dims[axis] {
                break;
            }
            at -= strides[axis] * dims[axis];
            index[axis] = 0;
        }
    }
    out
}

/// Row-major strides of `shape`.
fn strides(shape: &[usize]) -> Vec<usize> {
    let mut out = vec![1; shape.len()];
    for axis in (0..shape.len().saturating_sub(1)).rev() {
        out[axis] = out[axis + 1] * shape[axis + 1];
    }
    out
}

fn binary<T: Copy, U>(
    a: &[T],
    a_shape: &[usize],
    b: &[T],
    b_shape: &[usize],
    f: impl Fn(T, T) -> Result<U, String>,
) -> Result<(Vec<usize>, Vec<U>), String> {
    if a_shape == b_shape {
        let out = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| f(x, y))
            .collect::<Result<_, _>>()?;
        return Ok((a_shape.to_vec(), out));
    }
    let shape = broadcast_shape(&[a_shape, b_shape])?;
    let out = if b.len() == 1 && shape == a_shape {
        a.iter().map(|&x| f(x, b[0])).collect::<Result<_, _>>()?
    } else {
        let ai = broadcast_indices(a_shape, &shape);
        let bi = broadcast_indices(b_shape, &shape);
        ai.iter()
            .zip(&bi)
            .map(|(&i, &j)| f(a[i], b[j]))
            .collect::<Result<_, _>>()?
    };
    Ok((shape, out))
}

fn arithmetic(
    a: &Tensor,
    b: &Tensor,
    activation: Activation,
    float: impl Fn(f32, f32) -> f32,
    int: impl Fn(i32, i32) -> Result<i32, String>,
) -> Result<Tensor, String> {
    match (&a.data, &b.data) {
        (Data::F32(x), Data::F32(y)) => {
            let (shape, mut out) = binary(x, &a.shape, y, &b.shape, |p, q| Ok(float(p, q)))?;
            activation.apply(&mut out);
            Ok(Tensor::f32(shape, out))
        }
        (Data::I32(x), Data::I32(y)) => {
            if activation != Activation::None {
                return Err("a fused activation on integers is not supported".into());
            }
            let (shape, out) = binary(x, &a.shape, y, &b.shape, int)?;
            Ok(Tensor::i32(shape, out))
        }
        _ => Err("the operands are not both float32 or both int32".into()),
    }
}

fn greater(a: &Tensor, b: &Tensor) -> Result<Tensor, String> {
    let (shape, out) = match (&a.data, &b.data) {
        (Data::F32(x), Data::F32(y)) => binary(x, &a.shape, y, &b.shape, |p, q| Ok(p > q))?,
        (Data::I32(x), Data::I32(y)) => binary(x, &a.shape, y, &b.shape, |p, q| Ok(p > q))?,
        _ => return Err("the operands are not both float32 or both int32".into()),
    };
    Tensor::new(shape, Data::Bool(out))
}

fn unary(input: &Tensor, f: impl Fn(f32) -> f32) -> Result<Tensor, String> {
    Ok(Tensor::f32(
        input.shape.clone(),
        input.floats()?.iter().map(|&x| f(x)).collect(),
    ))
}

fn reshape(input: &Tensor, shape: &[i32]) -> Result<Tensor, String> {
    let total = input.data.len();
    let mut unknown = None;
    let mut known = 1usize;
    let mut dims = Vec::with_capacity(shape.len());
    for (axis, &dim) in shape.iter().enumerate() {
        if dim == -1 {
            if unknown.replace(axis).is_some() {
                return Err("two dimensions are -1".into());
            }
            dims.push(0);
        } else {
            let dim = usize::try_from(dim).map_err(|_| format!("dimension {dim} is negative"))?;
            known *= dim;
            dims.push(dim);
        }
    }
    if let Some(axis) = unknown {
        if known == 0 || !total.is_multiple_of(known) {
            return Err(format!("{total} values cannot take the shape {shape:?}"));
        }
        dims[axis] = total / known;
    }
    Tensor::new(dims, input.data.clone())
}

#[allow(clippy::too_many_arguments)]
fn strided_slice(
    input: &Tensor,
    begin: &[i32],
    end: &[i32],
    step: &[i32],
    begin_mask: i32,
    end_mask: i32,
    shrink_axis_mask: i32,
    offset: bool,
) -> Result<Tensor, String> {
    let rank = input.shape.len();
    if begin.len() > rank || end.len() != begin.len() || step.len() != begin.len() {
        return Err("the slice does not fit the input".into());
    }
    let mut starts = Vec::with_capacity(rank);
    let mut counts = Vec::with_capacity(rank);
    let mut steps = Vec::with_capacity(rank);
    let mut shape = Vec::new();
    for axis in 0..rank {
        let size = input.shape[axis] as i64;
        let bit = 1 << axis;
        let (start, count, stride) = if axis >= begin.len() {
            (0, size, 1)
        } else {
            let stride = step[axis] as i64;
            if stride == 0 {
                return Err("a slice step is zero".into());
            }
            let clamp = |value: i64| {
                if stride > 0 {
                    value.clamp(0, size)
                } else {
                    value.clamp(-1, size - 1)
                }
            };
            let start = if begin_mask & bit != 0 {
                if stride > 0 {
                    0
                } else {
                    size - 1
                }
            } else {
                let value = begin[axis] as i64;
                clamp(if value < 0 { value + size } else { value })
            };
            let stop = if shrink_axis_mask & bit != 0 {
                start + 1
            } else if end_mask & bit != 0 {
                if stride > 0 {
                    size
                } else {
                    -1
                }
            } else {
                let value = end[axis] as i64 + if offset { begin[axis] as i64 } else { 0 };
                clamp(if value < 0 && !offset {
                    value + size
                } else {
                    value
                })
            };
            let count = if stride > 0 {
                (stop - start + stride - 1).div_euclid(stride)
            } else {
                (start - stop - stride - 1).div_euclid(-stride)
            };
            (start, count.max(0), stride)
        };
        if shrink_axis_mask & bit != 0 && axis < begin.len() {
            if count != 1 || start < 0 || start >= size {
                return Err("a shrunk axis is out of range".into());
            }
        } else {
            shape.push(count as usize);
        }
        starts.push(start);
        counts.push(count as usize);
        steps.push(stride);
    }
    let input_strides = strides(&input.shape);
    let total: usize = counts.iter().product();
    let mut indices = Vec::with_capacity(total);
    let mut index = vec![0usize; rank];
    for _ in 0..total {
        let at: i64 = (0..rank)
            .map(|axis| {
                (starts[axis] + index[axis] as i64 * steps[axis]) * input_strides[axis] as i64
            })
            .sum();
        indices.push(at as usize);
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < counts[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    Tensor::new(shape, input.data.pick(&indices))
}

fn axis_of(axis: i32, rank: usize) -> Result<usize, String> {
    let value = if axis < 0 { axis + rank as i32 } else { axis };
    usize::try_from(value)
        .ok()
        .filter(|&value| value < rank)
        .ok_or_else(|| format!("axis {axis} is out of range for rank {rank}"))
}

fn pack(parts: &[&Tensor], axis: i32) -> Result<Tensor, String> {
    let first = parts.first().ok_or("nothing to pack")?;
    if parts.iter().any(|part| part.shape != first.shape) {
        return Err("the packed tensors differ in shape".into());
    }
    let axis = axis_of(axis, first.shape.len() + 1)?;
    let outer: usize = first.shape[..axis].iter().product();
    let inner: usize = first.shape[axis..].iter().product();
    let joined = Data::join(&parts.iter().map(|part| &part.data).collect::<Vec<_>>())?;
    let mut indices = Vec::with_capacity(joined.len());
    for o in 0..outer {
        for k in 0..parts.len() {
            let base = k * outer * inner + o * inner;
            indices.extend(base..base + inner);
        }
    }
    let mut shape = first.shape.clone();
    shape.insert(axis, parts.len());
    Tensor::new(shape, joined.pick(&indices))
}

fn concatenate(parts: &[&Tensor], axis: i32) -> Result<Tensor, String> {
    let first = parts.first().ok_or("nothing to join")?;
    let axis = axis_of(axis, first.shape.len())?;
    let mut shape = first.shape.clone();
    shape[axis] = 0;
    for part in parts {
        if part.shape.len() != first.shape.len()
            || part
                .shape
                .iter()
                .enumerate()
                .any(|(i, &d)| i != axis && d != first.shape[i])
        {
            return Err("the joined tensors differ in shape".into());
        }
        shape[axis] += part.shape[axis];
    }
    let outer: usize = first.shape[..axis].iter().product();
    let inner: usize = first.shape[axis + 1..].iter().product();
    let joined = Data::join(&parts.iter().map(|part| &part.data).collect::<Vec<_>>())?;
    let mut starts = Vec::with_capacity(parts.len());
    let mut at = 0;
    for part in parts {
        starts.push(at);
        at += part.data.len();
    }
    let mut indices = Vec::with_capacity(joined.len());
    for o in 0..outer {
        for (part, start) in parts.iter().zip(&starts) {
            let chunk = part.shape[axis] * inner;
            let base = start + o * chunk;
            indices.extend(base..base + chunk);
        }
    }
    Tensor::new(shape, joined.pick(&indices))
}

fn split(input: &Tensor, axis: i32, count: usize) -> Result<Vec<Tensor>, String> {
    let axis = axis_of(axis, input.shape.len())?;
    let size = input.shape[axis];
    if count == 0 || !size.is_multiple_of(count) {
        return Err(format!("{size} does not split into {count}"));
    }
    let part = size / count;
    let mut shape = input.shape.clone();
    shape[axis] = part;
    let input_strides = strides(&input.shape);
    (0..count)
        .map(|k| {
            let indices = odometer(&shape, &input_strides, k * part * input_strides[axis]);
            Tensor::new(shape.clone(), input.data.pick(&indices))
        })
        .collect()
}

fn range(start: &Tensor, limit: &Tensor, delta: &Tensor) -> Result<Tensor, String> {
    match (&start.data, &limit.data, &delta.data) {
        (Data::I32(s), Data::I32(l), Data::I32(d)) => {
            let (s, l, d) = (
                *s.first().ok_or("no start")?,
                *l.first().ok_or("no limit")?,
                *d.first().ok_or("no delta")?,
            );
            if d == 0 || (l > s && d < 0) || (l < s && d > 0) {
                return Err("the range does not reach its limit".into());
            }
            let count = ((l as i64 - s as i64).abs() + (d as i64).abs() - 1) / (d as i64).abs();
            let values: Vec<i32> = (0..count).map(|i| s + i as i32 * d).collect();
            Ok(Tensor::i32(vec![values.len()], values))
        }
        (Data::F32(s), Data::F32(l), Data::F32(d)) => {
            let (s, l, d) = (
                *s.first().ok_or("no start")?,
                *l.first().ok_or("no limit")?,
                *d.first().ok_or("no delta")?,
            );
            if d == 0.0 || (l > s && d < 0.0) || (l < s && d > 0.0) {
                return Err("the range does not reach its limit".into());
            }
            let count = ((l - s) / d).abs().ceil() as usize;
            let values: Vec<f32> = (0..count).map(|i| s + i as f32 * d).collect();
            Ok(Tensor::f32(vec![values.len()], values))
        }
        _ => Err("the range is not all int32 or all float32".into()),
    }
}

fn cast(input: &Tensor, to: Kind) -> Result<Tensor, String> {
    let data = match (&input.data, to) {
        (data, kind) if data.kind() == kind => data.clone(),
        (Data::I32(v), Kind::F32) => Data::F32(v.iter().map(|&x| x as f32).collect()),
        (Data::F32(v), Kind::I32) => Data::I32(v.iter().map(|&x| x as i32).collect()),
        (Data::Bool(v), Kind::F32) => Data::F32(v.iter().map(|&x| x as i32 as f32).collect()),
        (Data::Bool(v), Kind::I32) => Data::I32(v.iter().map(|&x| x as i32).collect()),
        (Data::F32(v), Kind::Bool) => Data::Bool(v.iter().map(|&x| x != 0.0).collect()),
        (Data::I32(v), Kind::Bool) => Data::Bool(v.iter().map(|&x| x != 0).collect()),
        (data, kind) => return Err(format!("cannot cast {:?} to {kind:?}", data.kind())),
    };
    Tensor::new(input.shape.clone(), data)
}

fn pad(input: &Tensor, paddings: &[i32], zero: i64) -> Result<Tensor, String> {
    let rank = input.shape.len();
    if paddings.len() != rank * 2 || paddings.iter().any(|&p| p < 0) {
        return Err("the paddings do not fit the input".into());
    }
    let shape: Vec<usize> = (0..rank)
        .map(|axis| {
            input.shape[axis] + paddings[2 * axis] as usize + paddings[2 * axis + 1] as usize
        })
        .collect();
    let total: usize = shape.iter().product();
    let input_strides = strides(&input.shape);
    // The input with one padding value at its end, which every padded
    // place picks.
    let filler = input.data.len();
    let mut indices = Vec::with_capacity(total);
    let mut index = vec![0usize; rank];
    for _ in 0..total {
        let mut at = Some(0);
        for axis in 0..rank {
            let before = paddings[2 * axis] as usize;
            let i = index[axis];
            if i < before || i >= before + input.shape[axis] {
                at = None;
                break;
            }
            at = at.map(|at| at + (i - before) * input_strides[axis]);
        }
        indices.push(at.unwrap_or(filler));
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < shape[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    let padding = match input.data {
        Data::F32(_) => Data::F32(vec![0.0]),
        Data::I32(_) => Data::I32(vec![0]),
        Data::I8(_) => Data::I8(vec![zero as i8]),
        Data::Bool(_) => Data::Bool(vec![false]),
    };
    let joined = Data::join(&[&input.data, &padding])?;
    Tensor::new(shape, joined.pick(&indices))
}

fn transpose(input: &Tensor, permutation: &[i32]) -> Result<Tensor, String> {
    let rank = input.shape.len();
    if permutation.len() != rank {
        return Err("the permutation does not fit the input".into());
    }
    let axes = permutation
        .iter()
        .map(|&axis| axis_of(axis, rank))
        .collect::<Result<Vec<usize>, String>>()?;
    let mut seen = vec![false; rank];
    for &axis in &axes {
        if std::mem::replace(&mut seen[axis], true) {
            return Err("the permutation repeats an axis".into());
        }
    }
    let input_strides = strides(&input.shape);
    let shape: Vec<usize> = axes.iter().map(|&axis| input.shape[axis]).collect();
    let out_strides: Vec<usize> = axes.iter().map(|&axis| input_strides[axis]).collect();
    Tensor::new(
        shape.clone(),
        input.data.pick(&odometer(&shape, &out_strides, 0)),
    )
}

fn tile(input: &Tensor, multiples: &[i32]) -> Result<Tensor, String> {
    let rank = input.shape.len();
    if multiples.len() != rank || multiples.iter().any(|&m| m < 0) {
        return Err("the multiples do not fit the input".into());
    }
    let shape: Vec<usize> = (0..rank)
        .map(|axis| input.shape[axis] * multiples[axis] as usize)
        .collect();
    let input_strides = strides(&input.shape);
    let total: usize = shape.iter().product();
    let mut indices = Vec::with_capacity(total);
    let mut index = vec![0usize; rank];
    for _ in 0..total {
        indices.push(
            (0..rank)
                .map(|axis| (index[axis] % input.shape[axis]) * input_strides[axis])
                .sum(),
        );
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < shape[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    Tensor::new(shape, input.data.pick(&indices))
}

fn gather(params: &Tensor, indices: &Tensor, axis: i32) -> Result<Tensor, String> {
    let axis = axis_of(axis, params.shape.len())?;
    let positions = indices.ints()?;
    let outer: usize = params.shape[..axis].iter().product();
    let size = params.shape[axis];
    let inner: usize = params.shape[axis + 1..].iter().product();
    let mut picked = Vec::with_capacity(outer * positions.len() * inner);
    for o in 0..outer {
        for &position in positions {
            let position = usize::try_from(position)
                .ok()
                .filter(|&p| p < size)
                .ok_or_else(|| format!("index {position} is out of range"))?;
            let base = (o * size + position) * inner;
            picked.extend(base..base + inner);
        }
    }
    let mut shape = params.shape[..axis].to_vec();
    shape.extend_from_slice(&indices.shape);
    shape.extend_from_slice(&params.shape[axis + 1..]);
    Tensor::new(shape, params.data.pick(&picked))
}

fn dequantize(input: &Tensor, quantization: &Quantization) -> Result<Tensor, String> {
    let Data::I8(values) = &input.data else {
        return Err("only int8 values are dequantized".into());
    };
    let per_channel = quantization.scales.len() > 1;
    let (channels, inner) = if per_channel {
        let dimension = quantization.dimension;
        if dimension >= input.shape.len() || input.shape[dimension] != quantization.scales.len() {
            return Err("the scales do not fit the input".into());
        }
        (
            input.shape[dimension],
            input.shape[dimension + 1..].iter().product::<usize>(),
        )
    } else {
        (1, 1)
    };
    let out = values
        .iter()
        .enumerate()
        .map(|(at, &value)| {
            let (scale, zero) = quantization.channel((at / inner) % channels);
            (value as i64 - zero) as f32 * scale
        })
        .collect();
    Ok(Tensor::f32(input.shape.clone(), out))
}

fn mean(input: &Tensor, axes: &[i32], keep_dims: bool) -> Result<Tensor, String> {
    let values = input.floats()?;
    let rank = input.shape.len();
    let mut reduced = vec![false; rank];
    for &axis in axes {
        reduced[axis_of(axis, rank)?] = true;
    }
    let kept_shape: Vec<usize> = (0..rank)
        .map(|axis| if reduced[axis] { 1 } else { input.shape[axis] })
        .collect();
    let out_strides = strides(&kept_shape);
    let to_out: Vec<usize> = (0..rank)
        .map(|axis| if reduced[axis] { 0 } else { out_strides[axis] })
        .collect();
    let mut sums = vec![0f32; kept_shape.iter().product()];
    for (value, at) in values.iter().zip(odometer(&input.shape, &to_out, 0)) {
        sums[at] += value;
    }
    let count: usize = (0..rank)
        .filter(|&axis| reduced[axis])
        .map(|axis| input.shape[axis])
        .product();
    for sum in &mut sums {
        *sum /= count as f32;
    }
    let shape = if keep_dims {
        kept_shape
    } else {
        (0..rank)
            .filter(|&axis| !reduced[axis])
            .map(|axis| input.shape[axis])
            .collect()
    };
    Ok(Tensor::f32(shape, sums))
}

/// The float values of `row` quantized to int8 as TFLite's
/// `SymmetricQuantizeFloats` / `AsymmetricQuantizeFloats` do: the values,
/// the scale and the zero point.
fn quantize_row(row: &[f32], asymmetric: bool, out: &mut Vec<i8>) -> (f32, i32) {
    out.clear();
    let low = row.iter().copied().fold(f32::INFINITY, f32::min);
    let high = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if asymmetric {
        let rmin = low.min(0.0) as f64;
        let rmax = high.max(0.0) as f64;
        if rmin == rmax {
            out.resize(row.len(), 0);
            return (1.0, 0);
        }
        let (qmin, qmax) = (-128.0f64, 127.0f64);
        let scale = (rmax - rmin) / (qmax - qmin);
        let from_min = qmin - rmin / scale;
        let from_max = qmax - rmax / scale;
        let error_min = qmin.abs() + (rmin / scale).abs();
        let error_max = qmax.abs() + (rmax / scale).abs();
        let zero = if error_min < error_max {
            from_min
        } else {
            from_max
        };
        let zero = if zero <= qmin {
            -128
        } else if zero >= qmax {
            127
        } else {
            zero.round() as i32
        };
        let scale = scale as f32;
        let inverse = 1.0 / scale;
        out.extend(
            row.iter()
                .map(|&x| ((zero as f32 + x * inverse).round() as i32).clamp(-128, 127) as i8),
        );
        (scale, zero)
    } else {
        let range = low.abs().max(high.abs());
        if range == 0.0 {
            out.resize(row.len(), 0);
            return (1.0, 0);
        }
        let scale = range / 127.0;
        out.extend(
            row.iter()
                .map(|&x| ((x * 127.0 / range).round() as i32).clamp(-127, 127) as i8),
        );
        (scale, 0)
    }
}

fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    let mut sums = [0i32; 16];
    let (a_chunks, a_rest) = a.as_chunks::<16>();
    let (b_chunks, b_rest) = b.as_chunks::<16>();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..16 {
            sums[lane] += x[lane] as i16 as i32 * y[lane] as i16 as i32;
        }
    }
    let mut total: i32 = sums.iter().sum();
    for (&x, &y) in a_rest.iter().zip(b_rest) {
        total += x as i32 * y as i32;
    }
    total
}

fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let mut sums = [0f32; 8];
    let (a_chunks, a_rest) = a.as_chunks::<8>();
    let (b_chunks, b_rest) = b.as_chunks::<8>();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            sums[lane] += x[lane] * y[lane];
        }
    }
    let mut total: f32 = sums.iter().sum();
    for (&x, &y) in a_rest.iter().zip(b_rest) {
        total += x * y;
    }
    total
}

/// Splits `units` output units across threads when the work is large
/// enough to be worth it; `work` fills the outputs of a range of units for
/// every row.
fn by_units(
    units: usize,
    work_size: usize,
    rows: usize,
    work: impl Fn(std::ops::Range<usize>, &mut [f32]) + Sync,
) -> Vec<f32> {
    let threads = if work_size >= 1 << 20 {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8)
    } else {
        1
    };
    if threads <= 1 {
        let mut out = vec![0f32; rows * units];
        work(0..units, &mut out);
        return out;
    }
    let chunk = units.div_ceil(threads);
    let parts: Vec<(std::ops::Range<usize>, Vec<f32>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..units)
            .step_by(chunk)
            .map(|start| {
                let range = start..(start + chunk).min(units);
                let work = &work;
                scope.spawn(move || {
                    let mut out = vec![0f32; rows * range.len()];
                    work(range.clone(), &mut out);
                    (range, out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a worker thread panicked"))
            .collect()
    });
    let mut out = vec![0f32; rows * units];
    for (range, part) in parts {
        let width = range.len();
        for row in 0..rows {
            out[row * units + range.start..row * units + range.end]
                .copy_from_slice(&part[row * width..(row + 1) * width]);
        }
    }
    out
}

/// `FULLY_CONNECTED`: float weights, or int8 weights with float inputs
/// quantized row by row (TFLite's hybrid kernel, `EvalHybrid`).
fn fully_connected(
    input: &Tensor,
    weights: &Tensor,
    bias: Option<&Tensor>,
    quantized: Option<(&Quantization, Option<&[i32]>)>,
    activation: Activation,
    keep_num_dims: bool,
    asymmetric: bool,
) -> Result<Tensor, String> {
    let x = input.floats()?;
    let [units, depth] = weights.shape[..] else {
        return Err("the weights are not a matrix".into());
    };
    if depth == 0 || x.len() % depth != 0 {
        return Err("the input does not fit the weights".into());
    }
    let rows = x.len() / depth;
    let bias = match bias {
        Some(bias) => {
            let values = bias.floats()?;
            if values.len() != units {
                return Err("the bias does not fit the weights".into());
            }
            Some(values)
        }
        None => None,
    };
    let mut out = match (&weights.data, quantized) {
        (Data::F32(w), _) => by_units(units, units * depth * rows, rows, |range, out| {
            let width = range.len();
            for row in 0..rows {
                let xr = &x[row * depth..(row + 1) * depth];
                for (k, unit) in range.clone().enumerate() {
                    out[row * width + k] = dot_f32(xr, &w[unit * depth..(unit + 1) * depth]);
                }
            }
        }),
        (Data::I8(w), Some((quantization, sums))) => {
            if quantization.zero_points.iter().any(|&zero| zero != 0) {
                return Err("the int8 weights are not symmetric".into());
            }
            if quantization.scales.len() != 1 && quantization.scales.len() != units {
                return Err("the weight scales do not fit the weights".into());
            }
            let mut quantized_rows = Vec::with_capacity(rows);
            let mut buffer = Vec::with_capacity(depth);
            for row in 0..rows {
                let (scale, zero) =
                    quantize_row(&x[row * depth..(row + 1) * depth], asymmetric, &mut buffer);
                quantized_rows.push((buffer.clone(), scale, zero));
            }
            let computed;
            let sums = match sums {
                Some(sums) => sums,
                None => {
                    computed = w
                        .chunks(depth)
                        .map(|row| row.iter().map(|&v| v as i32).sum())
                        .collect::<Vec<i32>>();
                    &computed
                }
            };
            by_units(units, units * depth * rows, rows, |range, out| {
                let width = range.len();
                for (row, (q, scale, zero)) in quantized_rows.iter().enumerate() {
                    for (k, unit) in range.clone().enumerate() {
                        let mut dot = dot_i8(q, &w[unit * depth..(unit + 1) * depth]);
                        if *zero != 0 {
                            dot -= sums[unit] * zero;
                        }
                        out[row * width + k] = dot as f32 * (scale * quantization.channel(unit).0);
                    }
                }
            })
        }
        _ => return Err("unsupported weight type".into()),
    };
    if let Some(bias) = bias {
        for row in out.chunks_mut(units) {
            for (value, b) in row.iter_mut().zip(bias) {
                *value += b;
            }
        }
    }
    activation.apply(&mut out);
    let shape = if keep_num_dims {
        let mut shape = input.shape.clone();
        *shape.last_mut().ok_or("the input is a scalar")? = units;
        shape
    } else {
        vec![rows, units]
    };
    Ok(Tensor::f32(shape, out))
}

fn batch_matmul(lhs: &Tensor, rhs: &Tensor, adj_x: bool, adj_y: bool) -> Result<Tensor, String> {
    let (a, b) = (lhs.floats()?, rhs.floats()?);
    if lhs.shape.len() < 2 || rhs.shape.len() < 2 {
        return Err("batch matmul needs matrices".into());
    }
    let (la, lb) = (lhs.shape.len(), rhs.shape.len());
    let (m, k) = if adj_x {
        (lhs.shape[la - 1], lhs.shape[la - 2])
    } else {
        (lhs.shape[la - 2], lhs.shape[la - 1])
    };
    let (k2, n) = if adj_y {
        (rhs.shape[lb - 1], rhs.shape[lb - 2])
    } else {
        (rhs.shape[lb - 2], rhs.shape[lb - 1])
    };
    if k != k2 {
        return Err(format!("inner sizes {k} and {k2} differ"));
    }
    let batch = broadcast_shape(&[&lhs.shape[..la - 2], &rhs.shape[..lb - 2]])?;
    let ai = broadcast_indices(&lhs.shape[..la - 2], &batch);
    let bi = broadcast_indices(&rhs.shape[..lb - 2], &batch);
    let mut out = vec![0f32; ai.len() * m * n];
    let mut left = vec![0f32; m * k];
    let mut right = vec![0f32; k * n];
    for (index, (&ia, &ib)) in ai.iter().zip(&bi).enumerate() {
        let sa = &a[ia * m * k..(ia + 1) * m * k];
        let sb = &b[ib * k * n..(ib + 1) * k * n];
        for i in 0..m {
            for p in 0..k {
                left[i * k + p] = if adj_x { sa[p * m + i] } else { sa[i * k + p] };
            }
        }
        for p in 0..k {
            for j in 0..n {
                right[p * n + j] = if adj_y { sb[j * k + p] } else { sb[p * n + j] };
            }
        }
        let target = &mut out[index * m * n..(index + 1) * m * n];
        for i in 0..m {
            let row = &mut target[i * n..(i + 1) * n];
            for p in 0..k {
                let value = left[i * k + p];
                for (slot, r) in row.iter_mut().zip(&right[p * n..(p + 1) * n]) {
                    *slot += value * r;
                }
            }
        }
    }
    let mut shape = batch;
    shape.extend([m, n]);
    Ok(Tensor::f32(shape, out))
}

/// `SELECT`: the condition has the shape of the values, is a scalar, or
/// picks whole rows along the first dimension.
fn select(condition: &Tensor, x: &Tensor, y: &Tensor) -> Result<Tensor, String> {
    let picks = condition.bools()?;
    if x.shape != y.shape {
        return Err("the two choices differ in shape".into());
    }
    let total = x.data.len();
    let inner = if condition.shape == x.shape {
        1
    } else if condition.shape.is_empty() {
        total.max(1)
    } else if condition.shape.len() == 1 && x.shape.first() == Some(&condition.shape[0]) {
        total / condition.shape[0].max(1)
    } else {
        return Err("the condition does not fit the choices".into());
    };
    let indices: Vec<usize> = (0..total)
        .map(|at| {
            if picks[at / inner.max(1)] {
                at
            } else {
                total + at
            }
        })
        .collect();
    let joined = Data::join(&[&x.data, &y.data])?;
    Tensor::new(x.shape.clone(), joined.pick(&indices))
}

fn select_v2(condition: &Tensor, x: &Tensor, y: &Tensor) -> Result<Tensor, String> {
    let picks = condition.bools()?;
    let shape = broadcast_shape(&[&condition.shape, &x.shape, &y.shape])?;
    let ci = broadcast_indices(&condition.shape, &shape);
    let xi = broadcast_indices(&x.shape, &shape);
    let yi = broadcast_indices(&y.shape, &shape);
    let offset = x.data.len();
    let indices: Vec<usize> = (0..ci.len())
        .map(|at| {
            if picks[ci[at]] {
                xi[at]
            } else {
                offset + yi[at]
            }
        })
        .collect();
    let joined = Data::join(&[&x.data, &y.data])?;
    Tensor::new(shape, joined.pick(&indices))
}

/// Softmax, or its logarithm, along the last dimension.
fn softmax(input: &Tensor, beta: f32, log: bool) -> Result<Tensor, String> {
    let values = input.floats()?;
    let depth = *input.shape.last().ok_or("softmax of a scalar")?;
    let mut out = Vec::with_capacity(values.len());
    if depth == 0 {
        return Ok(Tensor::f32(input.shape.clone(), out));
    }
    for row in values.chunks(depth) {
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if log {
            let sum: f32 = row.iter().map(|&x| (x - max).exp()).sum();
            let log_sum = sum.ln();
            out.extend(row.iter().map(|&x| x - max - log_sum));
        } else {
            let exps: Vec<f32> = row.iter().map(|&x| ((x - max) * beta).exp()).collect();
            let sum: f32 = exps.iter().sum();
            out.extend(exps.iter().map(|&e| e / sum));
        }
    }
    Ok(Tensor::f32(input.shape.clone(), out))
}

/// A flatbuffer table: `at` is where it starts in `data`.
#[derive(Clone, Copy)]
struct Table<'a> {
    data: &'a [u8],
    at: usize,
}

fn read<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], String> {
    data.get(at..at.checked_add(N).ok_or("an offset overflows")?)
        .map(|bytes| bytes.try_into().unwrap())
        .ok_or_else(|| "the file is cut short".to_string())
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, String> {
    read::<4>(data, at).map(u32::from_le_bytes)
}

impl<'a> Table<'a> {
    fn root(data: &'a [u8]) -> Result<Self, String> {
        if data.get(4..8) != Some(b"TFL3") {
            return Err("not a TFLite file".into());
        }
        Ok(Self {
            data,
            at: u32_at(data, 0)? as usize,
        })
    }

    /// Where field `index` is stored, when it is.
    fn field(&self, index: usize) -> Result<Option<usize>, String> {
        let back = i32::from_le_bytes(read::<4>(self.data, self.at)?) as i64;
        let vtable = usize::try_from(self.at as i64 - back)
            .map_err(|_| "a table's layout is out of range")?;
        let length = u16::from_le_bytes(read::<2>(self.data, vtable)?) as usize;
        let slot = 4 + 2 * index;
        if slot + 2 > length {
            return Ok(None);
        }
        let offset = u16::from_le_bytes(read::<2>(self.data, vtable + slot)?) as usize;
        Ok((offset != 0).then_some(self.at + offset))
    }

    /// Follows the offset stored at field `index`.
    fn target(&self, index: usize) -> Result<Option<usize>, String> {
        match self.field(index)? {
            Some(at) => Ok(Some(
                at.checked_add(u32_at(self.data, at)? as usize)
                    .ok_or("an offset overflows")?,
            )),
            None => Ok(None),
        }
    }

    fn u8(&self, index: usize, default: u8) -> Result<u8, String> {
        match self.field(index)? {
            Some(at) => read::<1>(self.data, at).map(|b| b[0]),
            None => Ok(default),
        }
    }

    fn i32(&self, index: usize, default: i32) -> Result<i32, String> {
        match self.field(index)? {
            Some(at) => read::<4>(self.data, at).map(i32::from_le_bytes),
            None => Ok(default),
        }
    }

    fn u32(&self, index: usize, default: u32) -> Result<u32, String> {
        match self.field(index)? {
            Some(at) => u32_at(self.data, at),
            None => Ok(default),
        }
    }

    fn f32(&self, index: usize, default: f32) -> Result<f32, String> {
        match self.field(index)? {
            Some(at) => read::<4>(self.data, at).map(f32::from_le_bytes),
            None => Ok(default),
        }
    }

    fn table(&self, index: usize) -> Result<Option<Table<'a>>, String> {
        Ok(self.target(index)?.map(|at| Table {
            data: self.data,
            at,
        }))
    }

    /// The elements of the vector at field `index`, each `size` bytes.
    fn vector(&self, index: usize, size: usize) -> Result<Option<&'a [u8]>, String> {
        let Some(at) = self.target(index)? else {
            return Ok(None);
        };
        let count = u32_at(self.data, at)? as usize;
        let start = at + 4;
        let end = count
            .checked_mul(size)
            .and_then(|length| start.checked_add(length))
            .ok_or("a vector is too long")?;
        self.data
            .get(start..end)
            .map(Some)
            .ok_or_else(|| "a vector runs past the end".to_string())
    }

    fn bytes(&self, index: usize) -> Result<Option<&'a [u8]>, String> {
        self.vector(index, 1)
    }

    fn ints(&self, index: usize) -> Result<Option<Vec<i32>>, String> {
        Ok(self.vector(index, 4)?.map(|bytes| {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|w| i32::from_le_bytes(*w))
                .collect()
        }))
    }

    fn floats(&self, index: usize) -> Result<Option<Vec<f32>>, String> {
        Ok(self.vector(index, 4)?.map(|bytes| {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|w| f32::from_le_bytes(*w))
                .collect()
        }))
    }

    fn longs(&self, index: usize) -> Result<Option<Vec<i64>>, String> {
        Ok(self.vector(index, 8)?.map(|bytes| {
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|w| i64::from_le_bytes(*w))
                .collect()
        }))
    }

    fn string(&self, index: usize) -> Result<Option<String>, String> {
        Ok(self
            .bytes(index)?
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned()))
    }

    fn tables(&self, index: usize) -> Result<Vec<Table<'a>>, String> {
        let Some(at) = self.target(index)? else {
            return Ok(Vec::new());
        };
        let count = u32_at(self.data, at)? as usize;
        (0..count)
            .map(|i| {
                let slot = at + 4 + 4 * i;
                let target = slot
                    .checked_add(u32_at(self.data, slot)? as usize)
                    .ok_or("an offset overflows")?;
                Ok(Table {
                    data: self.data,
                    at: target,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(shape: &[usize], values: &[f32]) -> Tensor {
        Tensor::f32(shape.to_vec(), values.to_vec())
    }

    fn ints(shape: &[usize], values: &[i32]) -> Tensor {
        Tensor::i32(shape.to_vec(), values.to_vec())
    }

    fn close(a: &[f32], b: &[f32], tolerance: f32) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
    }

    #[test]
    fn elementwise_operators_broadcast() {
        let a = floats(&[2, 3], &[1.0, -2.0, 3.0, 4.0, 5.0, -6.0]);
        let b = floats(&[3], &[10.0, 20.0, 30.0]);
        let sum = arithmetic(&a, &b, Activation::None, |x, y| x + y, |x, y| Ok(x + y)).unwrap();
        assert_eq!(sum, floats(&[2, 3], &[11.0, 18.0, 33.0, 14.0, 25.0, 24.0]));
        let column = floats(&[2, 1], &[1.0, -1.0]);
        let product = arithmetic(
            &a,
            &column,
            Activation::Relu,
            |x, y| x * y,
            |x, y| Ok(x * y),
        )
        .unwrap();
        assert_eq!(product, floats(&[2, 3], &[1.0, 0.0, 3.0, 0.0, 0.0, 6.0]));
        let quotient = arithmetic(
            &ints(&[3], &[-7, 7, 6]),
            &ints(&[], &[2]),
            Activation::None,
            |x, y| x / y,
            |x, y| Ok((x as f64 / y as f64).floor() as i32),
        )
        .unwrap();
        assert_eq!(quotient, ints(&[3], &[-4, 3, 3]));
        assert_eq!(
            greater(&a, &floats(&[], &[0.0])).unwrap().data,
            Data::Bool(vec![true, false, true, true, true, false])
        );
        assert!(arithmetic(
            &a,
            &floats(&[2], &[1.0, 2.0]),
            Activation::None,
            |x, y| x + y,
            |x, y| Ok(x + y)
        )
        .is_err());
        let r = unary(&floats(&[2], &[4.0, 0.25]), |x| 1.0 / x.sqrt()).unwrap();
        assert_eq!(r, floats(&[2], &[0.5, 2.0]));
    }

    #[test]
    fn shapes_are_sliced_packed_and_reshaped() {
        let shape = ints(&[3], &[5, 6, 7]);
        // shape[1], shrunk to a scalar, as the graphs read sizes.
        let one = strided_slice(&shape, &[1], &[2], &[1], 0, 0, 1, false).unwrap();
        assert_eq!(one, ints(&[], &[6]));
        let m = ints(&[3, 4], &(0..12).collect::<Vec<i32>>());
        let rows = strided_slice(&m, &[1, 0], &[0, 4], &[1, 2], 2, 1, 0, false).unwrap();
        assert_eq!(rows, ints(&[2, 2], &[4, 6, 8, 10]));
        let reversed = strided_slice(&m, &[-1, 0], &[0, 1], &[-1, 1], 0, 1, 0, false).unwrap();
        assert_eq!(reversed, ints(&[3, 1], &[8, 4, 0]));
        let packed = pack(&[&ints(&[], &[2]), &ints(&[], &[-1])], 0).unwrap();
        assert_eq!(packed, ints(&[2], &[2, -1]));
        let pair = pack(&[&ints(&[2], &[1, 2]), &ints(&[2], &[3, 4])], 1).unwrap();
        assert_eq!(pair, ints(&[2, 2], &[1, 3, 2, 4]));
        let reshaped = reshape(&m, &[2, -1]).unwrap();
        assert_eq!(reshaped.shape, [2, 6]);
        assert!(reshape(&m, &[5, -1]).is_err());
        assert_eq!(
            range(&ints(&[], &[0]), &ints(&[], &[5]), &ints(&[], &[2])).unwrap(),
            ints(&[3], &[0, 2, 4])
        );
        assert_eq!(
            cast(&ints(&[2], &[1, -3]), Kind::F32).unwrap(),
            floats(&[2], &[1.0, -3.0])
        );
    }

    #[test]
    fn tensors_are_joined_split_and_moved() {
        let a = ints(&[2, 2], &[1, 2, 3, 4]);
        let b = ints(&[2, 1], &[5, 6]);
        let joined = concatenate(&[&a, &b], -1).unwrap();
        assert_eq!(joined, ints(&[2, 3], &[1, 2, 5, 3, 4, 6]));
        let parts = split(&ints(&[2, 4], &(0..8).collect::<Vec<i32>>()), 1, 2).unwrap();
        assert_eq!(parts[0], ints(&[2, 2], &[0, 1, 4, 5]));
        assert_eq!(parts[1], ints(&[2, 2], &[2, 3, 6, 7]));
        let t = transpose(&ints(&[2, 3], &[1, 2, 3, 4, 5, 6]), &[1, 0]).unwrap();
        assert_eq!(t, ints(&[3, 2], &[1, 4, 2, 5, 3, 6]));
        let tiled = tile(&ints(&[1, 2], &[1, 2]), &[2, 2]).unwrap();
        assert_eq!(tiled, ints(&[2, 4], &[1, 2, 1, 2, 1, 2, 1, 2]));
        let padded = pad(&floats(&[1, 2], &[1.0, 2.0]), &[0, 1, 1, 0], 0).unwrap();
        assert_eq!(padded, floats(&[2, 3], &[0.0, 1.0, 2.0, 0.0, 0.0, 0.0]));
        let table = Tensor {
            shape: vec![3, 2],
            data: Data::I8(vec![1, 2, 3, 4, 5, 6]),
        };
        let rows = gather(&table, &ints(&[2], &[2, 0]), 0).unwrap();
        assert_eq!(rows.shape, [2, 2]);
        assert_eq!(rows.data, Data::I8(vec![5, 6, 1, 2]));
        assert!(gather(&table, &ints(&[1], &[3]), 0).is_err());
    }

    #[test]
    fn reductions_and_softmax_follow_the_last_axis() {
        let x = floats(&[2, 2], &[1.0, 3.0, 2.0, 2.0]);
        assert_eq!(mean(&x, &[-1], true).unwrap(), floats(&[2, 1], &[2.0, 2.0]));
        assert_eq!(mean(&x, &[0], false).unwrap(), floats(&[2], &[1.5, 2.5]));
        let p = softmax(&x, 1.0, false).unwrap();
        let e = 1.0 / (1.0 + 2f32.exp());
        assert!(close(p.floats().unwrap(), &[e, 1.0 - e, 0.5, 0.5], 1e-6));
        let l = softmax(&x, 1.0, true).unwrap();
        assert!(close(
            l.floats().unwrap(),
            &[e.ln(), (1.0 - e).ln(), 0.5f32.ln(), 0.5f32.ln()],
            1e-6
        ));
    }

    #[test]
    fn hybrid_fully_connected_is_close_to_the_float_result() {
        let weights = vec![10i8, -20, 30, -40, 5, 15, -25, 127];
        let scale = 0.05;
        let real: Vec<f32> = weights.iter().map(|&w| w as f32 * scale).collect();
        let input = floats(&[2, 4], &[0.5, -1.0, 2.0, 0.25, 3.0, 0.0, -0.5, 1.0]);
        let bias = floats(&[2], &[1.0, -1.0]);
        let float = fully_connected(
            &input,
            &floats(&[2, 4], &real),
            Some(&bias),
            None,
            Activation::None,
            false,
            false,
        )
        .unwrap();
        let quantization = Quantization {
            scales: vec![scale],
            zero_points: vec![0],
            dimension: 0,
        };
        let int8 = Tensor {
            shape: vec![2, 4],
            data: Data::I8(weights.clone()),
        };
        for asymmetric in [false, true] {
            let hybrid = fully_connected(
                &input,
                &int8,
                Some(&bias),
                Some((&quantization, None)),
                Activation::None,
                false,
                asymmetric,
            )
            .unwrap();
            assert_eq!(hybrid.shape, [2, 2]);
            assert!(
                close(hybrid.floats().unwrap(), float.floats().unwrap(), 0.1),
                "{hybrid:?} {float:?}"
            );
        }
        // One scale per output row.
        let per_channel = Quantization {
            scales: vec![scale, 2.0 * scale],
            zero_points: vec![0, 0],
            dimension: 0,
        };
        let doubled: Vec<f32> = real
            .iter()
            .enumerate()
            .map(|(i, &w)| if i < 4 { w } else { 2.0 * w })
            .collect();
        let float = fully_connected(
            &input,
            &floats(&[2, 4], &doubled),
            None,
            None,
            Activation::Relu,
            true,
            false,
        )
        .unwrap();
        let hybrid = fully_connected(
            &input,
            &int8,
            None,
            Some((&per_channel, None)),
            Activation::Relu,
            true,
            true,
        )
        .unwrap();
        assert_eq!(hybrid.shape, [2, 2]);
        assert!(close(
            hybrid.floats().unwrap(),
            float.floats().unwrap(),
            0.1
        ));
        // Inputs on the quantization grid come out exact.
        let mut q = Vec::new();
        let (s, zero) = quantize_row(&[1.27, -0.635, 0.0], false, &mut q);
        assert!(
            (s - 0.01).abs() < 1e-6 && zero == 0 && q == [127, -64, 0],
            "{s} {q:?}"
        );
        let (s, zero) = quantize_row(&[0.0, 2.55], true, &mut q);
        assert!(
            (s - 0.01).abs() < 1e-6 && zero == -128 && q == [-128, 127],
            "{s} {zero} {q:?}"
        );
        assert_eq!(dot_i8(&[1; 40], &[-2; 40]), -80);
    }

    #[test]
    fn batches_of_matrices_multiply_with_broadcast_batches() {
        let a = floats(&[2, 1, 2], &[1.0, 2.0, 3.0, 4.0]);
        let b = floats(&[2, 2], &[1.0, 0.0, 1.0, 1.0]);
        let out = batch_matmul(&a, &b, false, false).unwrap();
        assert_eq!(out, floats(&[2, 1, 2], &[3.0, 2.0, 7.0, 4.0]));
        let bt = batch_matmul(&a, &b, false, true).unwrap();
        assert_eq!(bt, floats(&[2, 1, 2], &[1.0, 3.0, 3.0, 7.0]));
        let at = batch_matmul(
            &floats(&[2, 1], &[1.0, 2.0]),
            &floats(&[2, 1], &[3.0, 4.0]),
            true,
            false,
        )
        .unwrap();
        assert_eq!(at, floats(&[1, 1], &[11.0]));
    }

    #[test]
    fn select_picks_rows_or_broadcast_elements() {
        let x = floats(&[2, 2], &[1.0, 2.0, 3.0, 4.0]);
        let y = floats(&[2, 2], &[-1.0, -2.0, -3.0, -4.0]);
        let rows = Tensor {
            shape: vec![2],
            data: Data::Bool(vec![false, true]),
        };
        assert_eq!(
            select(&rows, &x, &y).unwrap(),
            floats(&[2, 2], &[-1.0, -2.0, 3.0, 4.0])
        );
        let column = Tensor {
            shape: vec![2, 1],
            data: Data::Bool(vec![true, false]),
        };
        let picked = select_v2(&column, &x, &floats(&[], &[0.0])).unwrap();
        assert_eq!(picked, floats(&[2, 2], &[1.0, 2.0, 0.0, 0.0]));
        let quantization = Quantization {
            scales: vec![0.5, 2.0],
            zero_points: vec![0, 1],
            dimension: 1,
        };
        let q = Tensor {
            shape: vec![2, 2],
            data: Data::I8(vec![2, 3, -2, 1]),
        };
        assert_eq!(
            dequantize(&q, &quantization).unwrap(),
            floats(&[2, 2], &[1.0, 4.0, -1.0, 0.0])
        );
    }

    /// A piece of a flatbuffer to write: tables and vectors are written
    /// after the table that points at them.
    enum Node {
        Table(Vec<(usize, Node)>),
        U8(u8),
        I32(i32),
        Ints(Vec<i32>),
        Bytes(Vec<u8>),
        Text(&'static str),
        Tables(Vec<Node>),
    }

    fn align(out: &mut Vec<u8>) {
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }

    fn put(out: &mut [u8], at: usize, value: u32) {
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Writes `node` and gives back where it starts.
    fn write(out: &mut Vec<u8>, node: &Node) -> usize {
        align(out);
        let start = out.len();
        match node {
            Node::Table(fields) => {
                let slots = fields.iter().map(|(index, _)| index + 1).max().unwrap_or(0);
                out.extend(((4 + 2 * slots) as u16).to_le_bytes());
                out.extend(((4 + 4 * fields.len()) as u16).to_le_bytes());
                let slot_at = out.len();
                out.extend(vec![0u8; 2 * slots]);
                align(out);
                let table = out.len();
                out.extend(((table - start) as i32).to_le_bytes());
                let mut later = Vec::new();
                for (k, (index, value)) in fields.iter().enumerate() {
                    let offset = (4 + 4 * k) as u16;
                    out[slot_at + 2 * index..slot_at + 2 * index + 2]
                        .copy_from_slice(&offset.to_le_bytes());
                    let field = out.len();
                    match value {
                        Node::U8(v) => out.extend([*v, 0, 0, 0]),
                        Node::I32(v) => out.extend(v.to_le_bytes()),
                        other => {
                            out.extend([0; 4]);
                            later.push((field, other));
                        }
                    }
                }
                for (field, child) in later {
                    let at = write(out, child);
                    put(out, field, (at - field) as u32);
                }
                table
            }
            Node::Ints(values) => {
                out.extend((values.len() as u32).to_le_bytes());
                values.iter().for_each(|v| out.extend(v.to_le_bytes()));
                start
            }
            Node::Bytes(bytes) => {
                out.extend((bytes.len() as u32).to_le_bytes());
                out.extend(bytes);
                start
            }
            Node::Text(text) => {
                out.extend((text.len() as u32).to_le_bytes());
                out.extend(text.as_bytes());
                out.push(0);
                start
            }
            Node::Tables(tables) => {
                out.extend((tables.len() as u32).to_le_bytes());
                let slots = out.len();
                out.extend(vec![0u8; 4 * tables.len()]);
                for (k, table) in tables.iter().enumerate() {
                    let at = write(out, table);
                    put(out, slots + 4 * k, (at - (slots + 4 * k)) as u32);
                }
                start
            }
            Node::U8(_) | Node::I32(_) => unreachable!("scalars live in tables"),
        }
    }

    /// A model that adds a constant with a fused relu and multiplies the
    /// sum by itself, with operator codes `codes`.
    fn model(codes: &[i32]) -> Vec<u8> {
        let tensor = |name: &'static str, buffer: i32| {
            Node::Table(vec![
                (0, Node::Ints(vec![1, 2])),
                (1, Node::U8(0)),
                (2, Node::I32(buffer)),
                (3, Node::Text(name)),
                (7, Node::Ints(vec![-1, 2])),
            ])
        };
        let operator = |code: i32, inputs: Vec<i32>, output: i32, options: Option<Node>| {
            let mut fields = vec![
                (0, Node::I32(code)),
                (1, Node::Ints(inputs)),
                (2, Node::Ints(vec![output])),
            ];
            if let Some(options) = options {
                fields.push((3, Node::U8(1)));
                fields.push((4, options));
            }
            Node::Table(fields)
        };
        let constant: Vec<u8> = [10f32, -20.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let codes = codes
            .iter()
            .map(|&code| {
                Node::Table(vec![
                    (0, Node::U8(code.min(127) as u8)),
                    (3, Node::I32(code)),
                ])
            })
            .collect();
        let graph = Node::Table(vec![
            (
                0,
                Node::Tables(vec![
                    tensor("serving/x:0", 0),
                    tensor("bias", 1),
                    tensor("sum", 0),
                    tensor("serving/out:0", 0),
                ]),
            ),
            (1, Node::Ints(vec![0])),
            (2, Node::Ints(vec![3])),
            (
                3,
                Node::Tables(vec![
                    operator(0, vec![0, 1], 2, Some(Node::Table(vec![(0, Node::U8(1))]))),
                    operator(1, vec![2, 2], 3, None),
                ]),
            ),
        ]);
        let root = Node::Table(vec![
            (0, Node::I32(3)),
            (1, Node::Tables(codes)),
            (2, Node::Tables(vec![graph])),
            (
                4,
                Node::Tables(vec![
                    Node::Table(vec![]),
                    Node::Table(vec![(0, Node::Bytes(constant))]),
                ]),
            ),
        ]);
        let mut out = vec![0, 0, 0, 0, b'T', b'F', b'L', b'3'];
        let at = write(&mut out, &root);
        put(&mut out, 0, at as u32);
        out
    }

    #[test]
    fn a_small_model_file_is_read_and_run() {
        let graph = Graph::parse(&model(&[0, 18])).unwrap();
        assert_eq!(graph.input("x").unwrap(), 0);
        assert_eq!(graph.output("out").unwrap(), 0);
        assert_eq!(graph.input_shape(0), [-1, 2]);
        let out = graph
            .run(vec![floats(&[3, 2], &[1.0, 1.0, -11.0, 21.0, 0.5, 30.0])])
            .unwrap();
        assert_eq!(
            out,
            [floats(&[3, 2], &[121.0, 0.0, 0.0, 1.0, 110.25, 100.0])]
        );
        assert!(graph.run(vec![ints(&[1, 2], &[1, 2])]).is_err());
        assert!(graph.run(Vec::new()).is_err());
        // A custom operator is refused when the file is read.
        assert!(Graph::parse(&model(&[0, 32]))
            .unwrap_err()
            .contains("not supported"));
        assert!(Graph::parse(b"not a model").is_err());
        let mut cut = model(&[0, 18]);
        cut.truncate(cut.len() / 2);
        assert!(Graph::parse(&cut).is_err());
    }
}
