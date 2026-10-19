//! Fixed-width cache-aligned state for the acquisition ranking hot path.

pub const CACHE_LINE_BYTES: usize = 64;

pub const FLAG_HAS_METADATA: u32 = 1 << 0;
pub const FLAG_HAS_CANDIDATE_URL: u32 = 1 << 1;
pub const FLAG_AUTHORIZED_FULL_TEXT: u32 = 1 << 2;
pub const FLAG_PDF_SIGNATURE_VERIFIED: u32 = 1 << 3;
pub const FLAG_JATS_AVAILABLE: u32 = 1 << 4;
pub const FLAG_RESTRICTED_BY_SOURCE: u32 = 1 << 5;
pub const KNOWN_FLAGS: u32 = FLAG_HAS_METADATA
    | FLAG_HAS_CANDIDATE_URL
    | FLAG_AUTHORIZED_FULL_TEXT
    | FLAG_PDF_SIGNATURE_VERIFIED
    | FLAG_JATS_AVAILABLE
    | FLAG_RESTRICTED_BY_SOURCE;

/// Strings, URLs, provider payloads and provenance are intentionally kept in cold storage.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontierNodeHot {
    pub paper_id: u64,
    pub citation_count: u32,
    pub venue_rank: u16,
    /// Zero means unknown; future dates receive no recency bonus.
    pub year: u16,
    pub estimated_size_kib: u32,
    pub source_id: u32,
    pub flags: u32,
    pub _reserved: [u8; 36],
}

impl Default for FrontierNodeHot {
    fn default() -> Self {
        Self {
            paper_id: 0,
            citation_count: 0,
            venue_rank: 0,
            year: 0,
            estimated_size_kib: 0,
            source_id: 0,
            flags: 0,
            _reserved: [0; 36],
        }
    }
}

impl FrontierNodeHot {
    #[inline]
    pub const fn has_flag(&self, flag: u32) -> bool {
        (self.flags & flag) != 0
    }
}

const _: () = assert!(core::mem::size_of::<FrontierNodeHot>() == CACHE_LINE_BYTES);
const _: () = assert!(core::mem::align_of::<FrontierNodeHot>() == CACHE_LINE_BYTES);
