//! Encoder for the search request `params` field: a base64-encoded
//! protobuf (the same bytes the web UI carries in its `sp` parameter).
//!
//! Layout, cross-checked byte-for-byte against the `sp` values
//! YouTube's own filter menu emits:
//! outer message { field 1: sort (varint), field 2: filters (message) },
//! filters message { field 1: upload_date, field 2: type,
//! field 3: duration }.
//! E.g. sort alone encodes as `08 <sort>`: `CAI=` is sort=2 (upload
//! date), `CAM=` sort=3 (view count), `CAE=` sort=1 (rating);
//! `EgIQAQ` is filters{2:1} (type=video), `EgIIAQ` is filters{1:1}
//! (uploaded in the last hour), `EgQIAhAB` is filters{1:2, 2:1}
//! (uploaded today, type=video), `EgQQARgB` is filters{2:1, 3:1}
//! (type=video, duration=short).

use base64::Engine;

/// Sort order for search results (outer field 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    #[default]
    Relevance = 0,
    Rating = 1,
    UploadDate = 2,
    ViewCount = 3,
}

impl SearchSort {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "relevance" => Some(Self::Relevance),
            "rating" => Some(Self::Rating),
            "upload_date" => Some(Self::UploadDate),
            "view_count" => Some(Self::ViewCount),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Rating => "rating",
            Self::UploadDate => "upload_date",
            Self::ViewCount => "view_count",
        }
    }
}

/// Upload-date filter (filters field 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadDate {
    Hour = 1,
    Today = 2,
    Week = 3,
    Month = 4,
    Year = 5,
}

impl UploadDate {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "hour" => Some(Self::Hour),
            "today" => Some(Self::Today),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }
}

/// Result-type filter (filters field 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultType {
    Video = 1,
    Channel = 2,
    Playlist = 3,
    Movie = 4,
}

impl ResultType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Self::Video),
            "channel" => Some(Self::Channel),
            "playlist" => Some(Self::Playlist),
            "movie" => Some(Self::Movie),
            _ => None,
        }
    }
}

/// Duration filter (filters field 3). The numbering follows YouTube's
/// own `sp` bytes: short=1, long=2, medium=3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duration {
    Short = 1,
    Long = 2,
    Medium = 3,
}

impl Duration {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "short" => Some(Self::Short),
            "medium" => Some(Self::Medium),
            "long" => Some(Self::Long),
            _ => None,
        }
    }
}

/// The full filter set for one search request.
#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub sort: SearchSort,
    pub upload_date: Option<UploadDate>,
    pub result_type: Option<ResultType>,
    pub duration: Option<Duration>,
}

fn push_varint(out: &mut Vec<u8>, field: u32, value: u64) {
    out.push((field << 3) as u8);
    let mut v = value;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

impl SearchFilters {
    pub fn is_default(&self) -> bool {
        self.sort == SearchSort::Relevance
            && self.upload_date.is_none()
            && self.result_type.is_none()
            && self.duration.is_none()
    }

    /// Serialise to the base64 `params` string, or None for the default
    /// filter set (the field is then omitted, as the app does).
    pub fn encode_params(&self) -> Option<String> {
        if self.is_default() {
            return None;
        }
        let mut filters = Vec::new();
        if let Some(u) = self.upload_date {
            push_varint(&mut filters, 1, u as u64);
        }
        if let Some(t) = self.result_type {
            push_varint(&mut filters, 2, t as u64);
        }
        if let Some(d) = self.duration {
            push_varint(&mut filters, 3, d as u64);
        }
        let mut outer = Vec::new();
        if self.sort != SearchSort::Relevance {
            push_varint(&mut outer, 1, self.sort as u64);
        }
        if !filters.is_empty() {
            outer.push(((2 << 3) | 2) as u8);
            outer.push(filters.len() as u8);
            outer.extend_from_slice(&filters);
        }
        Some(base64::engine::general_purpose::STANDARD.encode(outer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_only_matches_known_sp_bytes() {
        // YouTube's own `sp` for "sort by upload date" is CAI= (08 02).
        let f = SearchFilters {
            sort: SearchSort::UploadDate,
            ..Default::default()
        };
        assert_eq!(f.encode_params().as_deref(), Some("CAI="));
        // Sort by view count: CAM= (08 03).
        let f = SearchFilters {
            sort: SearchSort::ViewCount,
            ..Default::default()
        };
        assert_eq!(f.encode_params().as_deref(), Some("CAM="));
        // Sort by rating: CAE= (08 01).
        let f = SearchFilters {
            sort: SearchSort::Rating,
            ..Default::default()
        };
        assert_eq!(f.encode_params().as_deref(), Some("CAE="));
    }

    #[test]
    fn default_encodes_to_none() {
        assert_eq!(SearchFilters::default().encode_params(), None);
    }

    #[test]
    fn known_sp_bundles_match_byte_for_byte() {
        use base64::Engine as _;
        let enc = |f: &SearchFilters| f.encode_params().unwrap();
        // Type=video alone: EgIQAQ (12 02 10 01).
        let f = SearchFilters {
            result_type: Some(ResultType::Video),
            ..Default::default()
        };
        assert_eq!(enc(&f), "EgIQAQ==");
        // Uploaded in the last hour: EgIIAQ (12 02 08 01).
        let f = SearchFilters {
            upload_date: Some(UploadDate::Hour),
            ..Default::default()
        };
        assert_eq!(enc(&f), "EgIIAQ==");
        // Uploaded today + type=video: EgQIAhAB (12 04 08 02 10 01).
        let f = SearchFilters {
            upload_date: Some(UploadDate::Today),
            result_type: Some(ResultType::Video),
            ..Default::default()
        };
        assert_eq!(enc(&f), "EgQIAhAB");
        // Type=video + duration=short: EgQQARgB (12 04 10 01 18 01).
        let f = SearchFilters {
            result_type: Some(ResultType::Video),
            duration: Some(Duration::Short),
            ..Default::default()
        };
        assert_eq!(enc(&f), "EgQQARgB");
        // Duration alone decodes to filters{3:<d>}.
        let f = SearchFilters {
            duration: Some(Duration::Long),
            ..Default::default()
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(enc(&f))
            .unwrap();
        assert_eq!(bytes, vec![0x12, 0x02, 0x18, 0x02]);
    }
}
