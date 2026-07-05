#![allow(dead_code)]

mod gc;
mod hash;
mod leases;
mod legacy;
mod manifests;
mod objects;
mod paths;
mod profiles;
mod publish;
mod realisations;
mod roots;
mod transactions;

#[allow(unused_imports)]
pub use gc::{GcCandidate, GcObjectCounts, GcReport, GcRootCounts, gc_dry_run};
#[allow(unused_imports)]
pub use hash::{TreeHash, canonical_tree_hash};
pub use leases::acquire_process_lease;
#[allow(unused_imports)]
pub use legacy::{
    LegacyInstallManifestInput, LegacyInstallManifests, install_ref_manifest_path,
    remove_install_ref_manifest, write_legacy_install_manifests,
};
#[allow(unused_imports)]
pub use manifests::{
    CompatibilityRef, InstallRefManifest, InstallRefMode, ObjectManifest, ProfileManifest,
    ProvenanceRecord, RealisationManifest, StoreTransactionManifest, StoreTxnState, read_manifest,
    read_object_manifest, validate_object_manifest_for_tree, write_manifest,
    write_profile_manifest,
};
#[allow(unused_imports)]
pub use objects::{
    PublishedStoreObject, StoreObjectPublishInput, publish_store_object, store_object_path,
};
pub use paths::StoreRoot;
#[allow(unused_imports)]
pub use profiles::{
    ProfileGeneration, ProjectProfileInput, current_project_profile_generation,
    list_profile_generations, list_project_profile_generations,
    previous_project_profile_generation, project_profile_root, rollback_project_profile,
    write_project_profile_generation,
};
#[allow(unused_imports)]
pub use publish::{
    StagedStoreInstallPublication, StagedStoreInstallPublishInput, StoreRealisationMetadata,
    publish_staged_store_install,
};
#[allow(unused_imports)]
pub use realisations::{
    StoreRealisationInput, StoreRealisationPublication, publish_store_realisation,
    read_realisation_manifest, realisation_manifest_path,
};
#[allow(unused_imports)]
pub use roots::{RootScan, RootScanError, read_roots};
#[allow(unused_imports)]
pub use transactions::{StoreTxn, StoreTxnInput, read_transaction_manifest};

pub const SCHEMA_VERSION: u32 = 1;
pub const HASH_ALGORITHM_SHA256: &str = "sha256";
pub const OBJECT_MANIFEST_FILE: &str = ".nise-object.toml";
pub const PROFILE_MANIFEST_FILE: &str = ".nise-profile.toml";
