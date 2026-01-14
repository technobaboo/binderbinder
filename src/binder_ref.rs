#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BinderRef(pub u32);

impl BinderRef {
    pub fn from_raw(n: u32) -> Self {
        BinderRef(n)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }
}

impl Default for BinderRef {
    fn default() -> Self {
        BinderRef(0)
    }
}

impl std::fmt::Display for BinderRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BinderRef({})", self.0)
    }
}

#[test]
fn test_binder_ref() {
    let ref1 = BinderRef::from_raw(100);
    let ref2 = BinderRef::from_raw(200);
    assert_eq!(ref1.as_u32(), 100);
    assert_eq!(ref2.as_u32(), 200);
    assert_ne!(ref1, ref2);
}
