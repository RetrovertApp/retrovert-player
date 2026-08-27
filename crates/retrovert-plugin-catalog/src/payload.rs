//! Extracted payload trees: the loadable form of an installed generation.
//!
//! A generation as the updater publishes it holds the release set's archives,
//! verbatim. Loading needs the flat tree inside them, so each generation is
//! extracted once into `payloads/<generation id>/` beside the updater's own
//! root. Extraction lands in a staging directory and is renamed into place,
//! so a committed tree is always a complete one; a tree that exists is
//! trusted and never re-extracted.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use retrovert_updater::Installed;

/// The archive form every plugin artifact ships in.
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Where an extraction stages before its tree commits. Never a valid
/// generation id, so a crashed extraction cannot shadow one.
const STAGING_PREFIX: &str = ".staging-";

pub(crate) struct PayloadStore {
    root: PathBuf,
}

impl PayloadStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where the committed tree for `id` sits.
    pub(crate) fn dir_of(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// The library each artifact loads from, inside a committed tree.
    fn libraries(dir: &Path, artifacts: &[Installed]) -> Vec<PathBuf> {
        artifacts
            .iter()
            .map(|artifact| {
                dir.join(format!(
                    "{}_playback{}",
                    artifact.name,
                    std::env::consts::DLL_SUFFIX
                ))
            })
            .collect()
    }

    /// The loadable libraries for a generation, extracting its tree if absent.
    ///
    /// The error is a description for the activation record: extraction
    /// failing is a fact about one generation, never fatal to the catalog.
    pub(crate) fn ensure(
        &self,
        id: &str,
        generation_dir: &Path,
        artifacts: &[Installed],
    ) -> Result<Vec<PathBuf>, String> {
        let dir = self.dir_of(id);
        if !dir.is_dir() {
            self.extract(id, generation_dir, artifacts)?;
        }
        let libraries = Self::libraries(&dir, artifacts);
        for library in &libraries {
            // `symlink_metadata` does not follow links: an archive that ships
            // the library as a symlink would otherwise redirect the load
            // outside the tree, which `unpack` does not prevent.
            if !fs::symlink_metadata(library).is_ok_and(|meta| meta.is_file()) {
                return Err(format!(
                    "{} is missing from the extracted payload",
                    library.display()
                ));
            }
        }
        Ok(libraries)
    }

    fn extract(
        &self,
        id: &str,
        generation_dir: &Path,
        artifacts: &[Installed],
    ) -> Result<(), String> {
        let staging = self.root.join(format!("{STAGING_PREFIX}{id}"));
        // A crashed run's leftovers would otherwise merge into this one.
        if staging.exists() {
            fs::remove_dir_all(&staging)
                .map_err(|e| format!("could not clear {}: {e}", staging.display()))?;
        }
        fs::create_dir_all(&staging)
            .map_err(|e| format!("could not create {}: {e}", staging.display()))?;

        for artifact in artifacts {
            if !artifact.path.ends_with(ARCHIVE_SUFFIX) {
                return Err(format!(
                    "artifact {} is not a {ARCHIVE_SUFFIX} archive: {}",
                    artifact.name, artifact.path
                ));
            }
            let archive = generation_dir.join(&artifact.path);
            let opened = File::open(&archive)
                .map_err(|e| format!("could not open {}: {e}", archive.display()))?;
            let decoder = zstd::stream::read::Decoder::new(opened)
                .map_err(|e| format!("could not decode {}: {e}", archive.display()))?;
            // `unpack` refuses entries that would *write* outside the staging
            // tree, but it creates symlinks with their target verbatim, so the
            // tree can still contain a link that points out of it. `ensure`
            // rejects a library that is not a regular file for that reason.
            tar::Archive::new(decoder)
                .unpack(&staging)
                .map_err(|e| format!("could not unpack {}: {e}", archive.display()))?;
        }

        let dir = self.dir_of(id);
        fs::rename(&staging, &dir).map_err(|e| {
            format!(
                "could not commit {} to {}: {e}",
                staging.display(),
                dir.display()
            )
        })
    }

    /// Remove every committed tree but the ones `live` names, and every
    /// staging leftover.
    pub(crate) fn retain(&self, live: &[String]) {
        let live: HashSet<&str> = live.iter().map(String::as_str).collect();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if live.contains(name) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                if let Err(e) = fs::remove_dir_all(&path) {
                    log::warn!("catalog: could not remove payload {}: {e}", path.display());
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn archive_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        let tarred = builder.into_inner().unwrap();
        zstd::stream::encode_all(tarred.as_slice(), 0).unwrap()
    }

    fn installed(name: &str, path: &str) -> Installed {
        Installed {
            name: name.to_string(),
            target: None,
            version: None,
            revision: "abc1234".to_string(),
            sha256: "ab".repeat(32),
            size: 0,
            path: path.to_string(),
        }
    }

    fn lib_name(name: &str) -> String {
        format!("{}_playback{}", name, std::env::consts::DLL_SUFFIX)
    }

    #[test]
    fn an_archive_extracts_into_a_committed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let generation_dir = dir.path().join("generation");
        fs::create_dir_all(&generation_dir).unwrap();
        let archive = archive_with(&[
            (&lib_name("spu"), b"not really a library"),
            ("spu_data/table.bin", b"payload data"),
        ]);
        fs::write(generation_dir.join("spu.tar.zst"), archive).unwrap();

        let store = PayloadStore::new(dir.path().join("payloads"));
        let libraries = store
            .ensure("gen-a", &generation_dir, &[installed("spu", "spu.tar.zst")])
            .expect("an extracted payload");

        assert_eq!(libraries, vec![store.dir_of("gen-a").join(lib_name("spu"))]);
        assert!(libraries[0].is_file());
        assert!(store.dir_of("gen-a").join("spu_data/table.bin").is_file());
    }

    #[test]
    fn a_committed_tree_is_trusted_and_never_re_extracted() {
        let dir = tempfile::tempdir().unwrap();
        let store = PayloadStore::new(dir.path().join("payloads"));
        let committed = store.dir_of("gen-a");
        fs::create_dir_all(&committed).unwrap();
        fs::write(committed.join(lib_name("spu")), b"planted").unwrap();

        // The generation directory holds no archive at all: extraction would
        // fail, so returning the planted library proves it was not attempted.
        let libraries = store
            .ensure("gen-a", dir.path(), &[installed("spu", "spu.tar.zst")])
            .expect("the committed tree");
        assert_eq!(fs::read(&libraries[0]).unwrap(), b"planted");
    }

    #[test]
    fn a_missing_library_after_extraction_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let generation_dir = dir.path().join("generation");
        fs::create_dir_all(&generation_dir).unwrap();
        let archive = archive_with(&[("README", b"no library here")]);
        fs::write(generation_dir.join("spu.tar.zst"), archive).unwrap();

        let store = PayloadStore::new(dir.path().join("payloads"));
        let refused = store.ensure("gen-a", &generation_dir, &[installed("spu", "spu.tar.zst")]);
        assert!(refused.is_err(), "{refused:?}");
    }

    #[test]
    fn a_library_that_is_a_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let generation_dir = dir.path().join("generation");
        fs::create_dir_all(&generation_dir).unwrap();
        let outside = dir.path().join("outside.so");
        fs::write(&outside, b"a library the archive does not own").unwrap();

        // `unpack` creates this link verbatim: the target escapes the tree
        // even though nothing was written outside it.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name(&outside).unwrap();
        header.set_cksum();
        builder
            .append_data(&mut header, lib_name("spu"), std::io::empty())
            .unwrap();
        let archive =
            zstd::stream::encode_all(builder.into_inner().unwrap().as_slice(), 0).unwrap();
        fs::write(generation_dir.join("spu.tar.zst"), archive).unwrap();

        let store = PayloadStore::new(dir.path().join("payloads"));
        let refused = store.ensure("gen-a", &generation_dir, &[installed("spu", "spu.tar.zst")]);
        assert!(
            refused.is_err(),
            "a symlinked library must not load: {refused:?}"
        );
    }

    #[test]
    fn retain_keeps_the_live_trees_and_clears_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let store = PayloadStore::new(dir.path().join("payloads"));
        for id in ["gen-a", "gen-b", ".staging-gen-c"] {
            fs::create_dir_all(store.root.join(id)).unwrap();
        }

        store.retain(&["gen-a".to_string()]);

        assert!(store.dir_of("gen-a").is_dir());
        assert!(!store.dir_of("gen-b").exists());
        assert!(!store.root.join(".staging-gen-c").exists());
    }
}
