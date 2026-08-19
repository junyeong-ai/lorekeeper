//! What a Lorekeeper installation consists of, whether it is coherent, and how it is brought
//! to a release.
//!
//! The vault runs on one discipline: every page declares its format and what it was derived
//! from, a command checks those declarations, and another repairs what the check reports.
//! Nothing is inferred from how a body looks. That discipline used to stop at the vault
//! boundary — the binary, the agent skills, the pipeline scripts and the templates were four
//! separately published artifacts that declared nothing, so a scheduled install could fall a
//! year behind while `lore health` reported green, because freshness of ingested data is the
//! only thing that declared itself.
//!
//! The answer here is one step stronger than declaring: the skills, the pipelines and the
//! templates are COMPILED INTO the binary, so they cannot be a different version from it —
//! there is one artifact, and the deploy methods on [`Installation`] write the copies. What
//! is left to check is only whether a deployed copy still equals what this binary carries,
//! which is a byte comparison rather than a version string, and therefore has no reading in
//! which it is merely probably right.

mod archive;
mod coherence;
mod embedded;
mod layout;
mod release;
mod target;
mod update;
mod verify;

pub use archive::read_from_tar_gz;
pub use coherence::{
    ArtifactState, DeployedArtifact, DeployedGroup, InstallationReport, SchemaState,
};
pub use embedded::{
    EmbeddedFile, config_files, pipeline_files, pipeline_names, skill_files, skill_names,
    template_files,
};
pub use layout::{DeployFailure, Deployed, Installation, Prune, SkillLevel};
pub use release::{Latest, Provenance, REPO, ReleaseClient, archive_name, asset_url, parse_tag};
pub use target::{Archive, ReleaseTarget};
pub use update::{Decision, decide, install_binary, running_version};
pub use verify::{sha256_hex, verify_attestation, verify_sidecar};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DistError {
    /// The running platform, or the running installation, is outside what this path supports.
    /// Always names the alternative, because the caller cannot discover it from a refusal.
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Release(String),
    /// The bytes that arrived are not the bytes the release published. Never a warning: an
    /// unverified binary is the one thing this must not install.
    #[error("{0}")]
    Integrity(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl DistError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// The version of the binary this code is compiled into — the one thing every comparison in
/// this crate is made against.
pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("crate version is semver")
}
