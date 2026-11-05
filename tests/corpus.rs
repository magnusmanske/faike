//! End-to-end checks against real Commons uploads plus one synthetic
//! watermark encode. Each fixture is a small file under `tests/fixtures/`;
//! see the README there for source and licence.
//!
//! The tests run the built binary, so they cover the CLI, config loading and
//! the detector orchestration, not just the parsers.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    file: &'static str,
    min_score: u8,
    sources: &'static [&'static str],
    codes: &'static [&'static str],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        file: "c2pa-source-type.png",
        min_score: 80,
        sources: &["c2pa"],
        codes: &["c2pa.digital_source_type", "c2pa.software_agent"],
    },
    Fixture {
        file: "c2pa-firefly.jpg",
        min_score: 80,
        sources: &["c2pa"],
        codes: &["c2pa.software_agent"],
    },
    Fixture {
        file: "xmp-source-type.jpg",
        min_score: 80,
        sources: &["xmp"],
        codes: &["xmp.digital_source_type"],
    },
    Fixture {
        file: "xmp-meta.jpg",
        min_score: 80,
        sources: &["xmp"],
        codes: &["xmp.digital_source_type"],
    },
    Fixture {
        file: "exif-user-comment.png",
        min_score: 50,
        sources: &["exif"],
        codes: &["exif.user_comment"],
    },
    Fixture {
        file: "watermark-sd.png",
        min_score: 80,
        sources: &["watermark"],
        codes: &["watermark.dwt_dct.payload"],
    },
];

fn fixture(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(file)
}

/// A config without TrustMark models, so the tests do not depend on the
/// ~63 MB downloads or on whatever config the developer has locally.
fn config(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"detect":{"watermark":{"models_dir":"/nonexistent/aimer-models"}}}"#,
    )
    .expect("write test config");
    path
}

fn scan(config: &Path, file: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_aimer"))
        .arg("scan")
        .arg(file)
        .env("AIMER_CONFIG", config)
        .output()
        .expect("run aimer");
    assert!(
        output.status.success(),
        "aimer scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("scan output is JSON")
}

fn has_source(scan: &Value, source: &str) -> bool {
    scan["signals"]
        .as_array()
        .is_some_and(|signals| signals.iter().any(|signal| signal["source"] == source))
}

fn has_code(scan: &Value, code: &str) -> bool {
    scan["signals"]
        .as_array()
        .is_some_and(|signals| signals.iter().any(|signal| signal["code"] == code))
}

#[test]
fn commons_fixtures_produce_expected_signals() {
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    for case in FIXTURES {
        let path = fixture(case.file);
        let result = scan(&config, &path);
        let score = result["score"].as_u64().unwrap_or(0) as u8;
        assert!(
            score >= case.min_score,
            "{}: score {score} < {} ({result})",
            case.file,
            case.min_score
        );
        for source in case.sources {
            assert!(
                has_source(&result, source),
                "{}: missing source {source} ({result})",
                case.file
            );
        }
        for code in case.codes {
            assert!(
                has_code(&result, code),
                "{}: missing code {code} ({result})",
                case.file
            );
        }
    }
}

#[test]
fn ordinary_image_stays_quiet() {
    let dir = TempDir::new().unwrap();
    let config = config(&dir);
    let result = scan(&config, &fixture("ordinary.jpg"));
    assert_eq!(result["score"], 0, "{result}");
    assert_eq!(
        result["signals"].as_array().map(Vec::len),
        Some(0),
        "{result}"
    );
}

#[test]
fn firefly_fixture_names_the_tool() {
    let dir = TempDir::new().unwrap();
    let result = scan(&config(&dir), &fixture("c2pa-firefly.jpg"));
    assert!(
        result["signals"].as_array().is_some_and(|signals| signals
            .iter()
            .any(|signal| signal["tool"] == "Adobe Firefly")),
        "{result}"
    );
}
