use std::sync::Arc;
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Plane {
    Gpu,
    Default,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Cell {
    pub plane: Plane,
    pub vpn: u32,
}

impl Cell {
    pub fn at(plane: Plane, va: u64) -> Option<Self> {
        (va >> 48 == 0 && va & 65535 == 0).then_some(Self {
            plane,
            vpn: (va >> 16) as u32,
        })
    }
}

pub(super) const UNMAPPED: u32 = 1 << 31;

pub(super) fn mapped_pte(index: u32, run_offset: usize) -> Option<u32> {
    if index == 0 || index >= 1 << 20 || run_offset % 65536 != 0 || run_offset / 65536 >= 128 {
        return None;
    }
    Some(UNMAPPED | ((run_offset as u32 >> 16) << 20) | index)
}

pub enum Target<B> {
    Inherit,
    Unmapped,
    Page(Arc<B>),
}
impl<B> Clone for Target<B> {
    fn clone(&self) -> Self {
        match self {
            Self::Inherit => Self::Inherit,
            Self::Unmapped => Self::Unmapped,
            Self::Page(p) => Self::Page(Arc::clone(p)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_nonzero_pte_never_names_an_arbitrary_descriptor_kind() {
        assert_eq!(mapped_pte(0, 0), None);
        assert_eq!(mapped_pte(1 << 20, 0), None);
        assert_eq!(mapped_pte(123, 1), None);
        assert_eq!(mapped_pte(123, 128 * 65536), None);
        assert_eq!(mapped_pte(123, 127 * 65536), Some(0x87f0007b));
        assert!(Cell::at(Plane::Gpu, 1 << 48).is_none());
        assert!(Cell::at(Plane::Gpu, 123).is_none());
    }

    /// The pages of one run share its view's index and differ in the page bits, as the
    /// translated shaders take them apart (`xcl_raw_gpuva`).
    #[test]
    fn pages_of_one_run_share_a_view_and_differ_in_the_page_bits() {
        let shader = |pte: u32| (pte & 0xfffff, ((pte >> 20) & 127) as usize * 65536);
        for page in [0usize, 1, 64, 127] {
            let pte = mapped_pte(77, page * 65536).unwrap();
            assert_eq!(shader(pte), (77, page * 65536));
            assert_ne!(pte & UNMAPPED, 0);
        }
        assert_eq!(mapped_pte(77, 8 << 20), None, "a run is 8 MiB at most");
    }
}
