/// Stable identifier for a persistent storage namespace owned by an extension.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExtensionStorageId {
    namespace: &'static str,
}

impl ExtensionStorageId {
    pub const fn new(namespace: &'static str) -> Self {
        Self { namespace }
    }

    pub const fn as_str(self) -> &'static str {
        self.namespace
    }
}

#[cfg(test)]
mod tests {
    use super::ExtensionStorageId;
    use std::collections::HashSet;

    #[test]
    fn storage_namespaces_preserve_const_identity_and_value_semantics() {
        const USAGE: ExtensionStorageId = ExtensionStorageId::new("usage-ledger");
        const ATTESTATION: ExtensionStorageId =
            ExtensionStorageId::new("memories.phase2-attestation");

        assert_eq!(USAGE.as_str(), "usage-ledger");
        assert_eq!(ATTESTATION.as_str(), "memories.phase2-attestation");
        assert_eq!(USAGE, ExtensionStorageId::new("usage-ledger"));
        assert_ne!(USAGE, ATTESTATION);
        assert_eq!(HashSet::from([USAGE, USAGE, ATTESTATION]).len(), 2);
    }
}
