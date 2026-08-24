//! Shared helpers for the LiteSVM integration suites.
//!
//! Every suite runs the REAL compiled `target/deploy/tangem_gacha_vault.so`.
//! `cargo test` does NOT rebuild that artifact — only `anchor build` /
//! `cargo build-sbf` does. Without a guard, editing `src/lib.rs` and running
//! `cargo test` silently exercises the PREVIOUS binary, so a broken change can
//! come back green and a new assertion can "pass" against code that was never
//! compiled. `program_so_path` fails loudly instead.
//!
//! Shared by all five suites, but not every suite uses every helper (only
//! sweep.rs decodes events), so the unused-item lint is off for this module.
#![allow(dead_code)]

use base64::Engine;
use std::path::PathBuf;
use std::time::SystemTime;

fn mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Path to the compiled program, checked for staleness against the sources it
/// is built from. Panics with an actionable message when a rebuild is due.
pub fn program_so_path() -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let so = root.join("../../target/deploy/tangem_gacha_vault.so");

    let so_time = mtime(&so).unwrap_or_else(|| {
        panic!(
            "{} does not exist — run `anchor build` first",
            so.display()
        )
    });

    // Cargo.toml matters too: a dependency or feature change alters the binary
    // without touching lib.rs.
    for src in ["src/lib.rs", "Cargo.toml"] {
        let path = root.join(src);
        if let Some(src_time) = mtime(&path) {
            if src_time > so_time {
                panic!(
                    "target/deploy/tangem_gacha_vault.so is OLDER than {src}.\n\
                     `cargo test` never rebuilds the .so, so this run would test \
                     the previous binary.\n\
                     Run `anchor build` first."
                );
            }
        }
    }

    so.to_string_lossy().into_owned()
}

/// Decodes the first Anchor event of type `T` out of a transaction's logs.
///
/// Anchor emits events as `Program data: <base64 of discriminator || borsh>`.
/// Nothing in the suite used to decode one, so an event's SHAPE (a renamed or
/// reordered field, a wrong value) was entirely unverified.
pub fn decode_event<T>(logs: &[String]) -> Option<T>
where
    T: anchor_lang::Discriminator + anchor_lang::AnchorDeserialize,
{
    let disc = T::DISCRIMINATOR;
    for line in logs {
        let Some(b64) = line.strip_prefix("Program data: ") else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
            continue;
        };
        if bytes.len() >= disc.len() && &bytes[..disc.len()] == disc {
            if let Ok(ev) = T::try_from_slice(&bytes[disc.len()..]) {
                return Some(ev);
            }
        }
    }
    None
}
