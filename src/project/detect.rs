//! Weighted, explainable project detection.
//!
//! `AGENTS.md` §4: detection must use multiple signals, be explainable, and be
//! overridable. So scoring never returns a bare boolean --- every point in a
//! score is attributable to a named [`Signal`] the UI can display.

use std::path::{Path, PathBuf};

use crate::backend::{BackendKind, BackendRegistry};
use crate::error::{Error, Result};
use crate::project::DirScan;

/// Minimum confidence for a directory to be considered a project at all.
pub const MIN_CONFIDENCE: f32 = 0.35;
/// Confidence at which a backend is selected without asking the user.
pub const AUTO_CONFIDENCE: f32 = 0.60;
/// Two candidates closer than this are reported as ambiguous.
pub const AMBIGUITY_MARGIN: f32 = 0.15;
/// How far up the tree the project root is searched for.
pub const MAX_SEARCH_DEPTH: usize = 32;

/// One piece of evidence found in a directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    /// Stable identifier, useful for tests and future config-based tuning.
    pub id: &'static str,
    /// How much this signal contributes to the backend's score.
    pub weight: f32,
    /// Human-readable justification shown in the UI.
    pub detail: &'static str,
}

impl Signal {
    pub const fn new(id: &'static str, weight: f32, detail: &'static str) -> Self {
        Self { id, weight, detail }
    }
}

/// A backend's total evidence for one directory.
#[derive(Debug, Clone)]
pub struct BackendScore {
    pub kind: BackendKind,
    pub score: f32,
    pub confidence: f32,
    pub signals: Vec<Signal>,
}

impl BackendScore {
    fn new(kind: BackendKind, signals: Vec<Signal>, saturation: f32) -> Self {
        // Folded from an explicit `0.0` rather than `sum()`, whose identity for
        // floats is `-0.0` --- which would render as "-0.00" for no evidence.
        let score = signals
            .iter()
            .fold(0.0_f32, |total, signal| total + signal.weight);
        let confidence = if saturation > 0.0 {
            (score / saturation).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            kind,
            score,
            confidence,
            signals,
        }
    }
}

/// What detection concluded about a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionOutcome {
    /// One backend is clearly ahead.
    Detected(BackendKind),
    /// Several plausible candidates; the user must choose (`SPEC.md` §7).
    Ambiguous(Vec<BackendKind>),
    /// No candidate reached [`MIN_CONFIDENCE`].
    Unknown,
}

impl DetectionOutcome {
    /// The backend to use, if detection settled on one.
    pub fn backend(&self) -> Option<BackendKind> {
        match self {
            Self::Detected(kind) => Some(*kind),
            Self::Ambiguous(_) | Self::Unknown => None,
        }
    }
}

/// Where the conclusion came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionSource {
    Automatic,
    /// Chosen by the user, overriding the heuristics.
    Manual,
}

/// The full result of a detection run, including the evidence behind it.
#[derive(Debug, Clone)]
pub struct Detection {
    /// Directory identified as the project root.
    pub root: PathBuf,
    pub outcome: DetectionOutcome,
    /// Every backend's score at [`Detection::root`], best first.
    pub scores: Vec<BackendScore>,
    pub source: DetectionSource,
    /// Directories examined, nearest first --- shown when detection fails.
    pub searched: Vec<PathBuf>,
}

impl Detection {
    /// The selected backend, or `None` when ambiguous/unknown.
    pub fn backend(&self) -> Option<BackendKind> {
        self.outcome.backend()
    }

    /// Confidence of `kind` at the detected root.
    pub fn confidence_of(&self, kind: BackendKind) -> f32 {
        self.scores
            .iter()
            .find(|score| score.kind == kind)
            .map_or(0.0, |score| score.confidence)
    }

    /// Confidence of the selected backend, if any.
    pub fn confidence(&self) -> Option<f32> {
        self.backend().map(|kind| self.confidence_of(kind))
    }

    /// Replaces the conclusion with a user-chosen backend, keeping the evidence
    /// so the UI can still show what the heuristics thought.
    pub fn overridden_with(mut self, kind: BackendKind) -> Self {
        self.outcome = DetectionOutcome::Detected(kind);
        self.source = DetectionSource::Manual;
        self
    }
}

/// Scores every backend against a single directory, best first.
pub fn score_directory(registry: &BackendRegistry, scan: &DirScan) -> Vec<BackendScore> {
    let mut scores: Vec<BackendScore> = registry
        .backends()
        .map(|backend| {
            BackendScore::new(backend.kind(), backend.detect(scan), backend.saturation())
        })
        .collect();

    // Ties broken by kind so the ordering is deterministic across runs.
    scores.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.kind.cmp(&b.kind))
    });
    scores
}

/// Classifies an ordered score list.
pub fn classify(scores: &[BackendScore]) -> DetectionOutcome {
    let Some(best) = scores.first() else {
        return DetectionOutcome::Unknown;
    };
    if best.confidence < MIN_CONFIDENCE {
        return DetectionOutcome::Unknown;
    }

    let contenders: Vec<BackendKind> = scores
        .iter()
        .filter(|score| {
            score.confidence >= MIN_CONFIDENCE
                && (best.confidence - score.confidence) < AMBIGUITY_MARGIN
        })
        .map(|score| score.kind)
        .collect();

    if contenders.len() > 1 {
        DetectionOutcome::Ambiguous(contenders)
    } else if best.confidence >= AUTO_CONFIDENCE {
        DetectionOutcome::Detected(best.kind)
    } else {
        // Above the noise floor but not convincing on its own: let the user confirm.
        DetectionOutcome::Ambiguous(vec![best.kind])
    }
}

/// Searches from `start` upward for the nearest directory that looks like a
/// project root (`SPEC.md` §7).
///
/// The nearest directory that reaches [`AUTO_CONFIDENCE`] wins. If none does,
/// the best-scoring ancestor is reported so the user sees *why* it failed
/// instead of a bare "unknown project".
pub fn detect_from(registry: &BackendRegistry, start: &Path) -> Result<Detection> {
    let mut searched = Vec::new();
    let mut best: Option<(Vec<BackendScore>, PathBuf)> = None;

    for dir in start.ancestors().take(MAX_SEARCH_DEPTH) {
        let scan = match DirScan::read(dir) {
            Ok(scan) => scan,
            // An unreadable ancestor (permissions, race) must not abort the
            // search; only failing on `start` itself is a real error.
            Err(source) if dir == start => {
                return Err(Error::ProjectScan {
                    path: dir.to_path_buf(),
                    source,
                });
            }
            Err(_) => break,
        };
        searched.push(dir.to_path_buf());

        let scores = score_directory(registry, &scan);
        let top = scores.first().map_or(0.0, |score| score.confidence);

        if top >= AUTO_CONFIDENCE {
            let outcome = classify(&scores);
            return Ok(Detection {
                root: dir.to_path_buf(),
                outcome,
                scores,
                source: DetectionSource::Automatic,
                searched,
            });
        }

        let better = best
            .as_ref()
            .is_none_or(|(current, _)| current.first().map_or(0.0, |s| s.confidence) < top);
        if better {
            best = Some((scores, dir.to_path_buf()));
        }
    }

    let (scores, root) = best.unwrap_or_else(|| (Vec::new(), start.to_path_buf()));
    let outcome = classify(&scores);
    Ok(Detection {
        root,
        outcome,
        scores,
        source: DetectionSource::Automatic,
        searched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> BackendRegistry {
        BackendRegistry::with_builtin_backends()
    }

    fn outcome_for(scan: &DirScan) -> DetectionOutcome {
        classify(&score_directory(&registry(), scan))
    }

    fn confidence_for(scan: &DirScan, kind: BackendKind) -> f32 {
        score_directory(&registry(), scan)
            .into_iter()
            .find(|score| score.kind == kind)
            .map_or(0.0, |score| score.confidence)
    }

    #[test]
    fn pyproject_alone_never_identifies_micropython() {
        // SPEC.md §19 acceptance criterion, and AGENTS.md §4.
        let scan = DirScan::from_parts(
            "/p",
            ["pyproject.toml"],
            [],
            [(
                "pyproject.toml",
                "[project]\nname = \"web-scraper\"\ndependencies = [\"requests\"]",
            )],
        );

        assert_eq!(confidence_for(&scan, BackendKind::MicroPython), 0.0);
        assert_eq!(outcome_for(&scan), DetectionOutcome::Unknown);
    }

    #[test]
    fn ordinary_python_project_is_not_micropython() {
        let scan = DirScan::from_parts(
            "/p",
            ["main.py", "utils.py", "pyproject.toml", "README.md"],
            ["tests"],
            [("pyproject.toml", "[project]\ndependencies = [\"flask\"]")],
        );

        assert!(confidence_for(&scan, BackendKind::MicroPython) < MIN_CONFIDENCE);
        assert_eq!(outcome_for(&scan), DetectionOutcome::Unknown);
    }

    #[test]
    fn boot_and_main_identify_micropython() {
        let scan = DirScan::from_parts("/p", ["boot.py", "main.py", "config.py"], ["lib"], []);

        assert_eq!(
            outcome_for(&scan),
            DetectionOutcome::Detected(BackendKind::MicroPython)
        );
        assert!(confidence_for(&scan, BackendKind::MicroPython) >= AUTO_CONFIDENCE);
    }

    #[test]
    fn pyproject_declaring_mpremote_identifies_micropython() {
        let scan = DirScan::from_parts(
            "/p",
            ["pyproject.toml", "app.py"],
            [],
            [(
                "pyproject.toml",
                "[tool.poetry.dependencies]\nmpremote = \"^1.22\"",
            )],
        );

        assert_eq!(
            outcome_for(&scan),
            DetectionOutcome::Detected(BackendKind::MicroPython)
        );
    }

    #[test]
    fn zephyr_application_is_detected_by_find_package() {
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt", "prj.conf"],
            ["src", "boards"],
            [(
                "CMakeLists.txt",
                "cmake_minimum_required(VERSION 3.20.0)\nfind_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})\nproject(blinky)",
            )],
        );

        assert_eq!(
            outcome_for(&scan),
            DetectionOutcome::Detected(BackendKind::Zephyr)
        );
        assert_eq!(confidence_for(&scan, BackendKind::Zephyr), 1.0);
    }

    #[test]
    fn west_workspace_root_is_detected() {
        let scan = DirScan::from_parts("/p", ["west.yml"], [".west", "zephyr", "modules"], []);
        assert_eq!(
            outcome_for(&scan),
            DetectionOutcome::Detected(BackendKind::Zephyr)
        );
    }

    #[test]
    fn plain_cmake_project_is_not_zephyr() {
        // AGENTS.md §4 / SPEC.md §7: distinguish a normal CMake project.
        let scan = DirScan::from_parts(
            "/p",
            ["CMakeLists.txt", "main.c"],
            ["src", "include"],
            [(
                "CMakeLists.txt",
                "project(hello)\nadd_executable(hello main.c)",
            )],
        );

        assert!(confidence_for(&scan, BackendKind::Zephyr) < MIN_CONFIDENCE);
        assert_eq!(outcome_for(&scan), DetectionOutcome::Unknown);
    }

    #[test]
    fn empty_directory_is_unknown() {
        let scan = DirScan::empty("/p");
        assert_eq!(outcome_for(&scan), DetectionOutcome::Unknown);
        assert!(
            score_directory(&registry(), &scan)
                .iter()
                .all(|s| s.signals.is_empty())
        );
    }

    #[test]
    fn overlapping_evidence_is_reported_as_ambiguous() {
        // A Zephyr app that also carries MicroPython scripts: neither wins.
        let scan = DirScan::from_parts(
            "/p",
            [
                "boot.py",
                "main.py",
                "prj.conf",
                "west.yml",
                "pyproject.toml",
            ],
            [".west"],
            [("pyproject.toml", "dependencies = [\"mpremote\"]")],
        );

        match outcome_for(&scan) {
            DetectionOutcome::Ambiguous(kinds) => {
                assert!(kinds.contains(&BackendKind::MicroPython));
                assert!(kinds.contains(&BackendKind::Zephyr));
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn a_weak_lone_candidate_asks_for_confirmation() {
        // Between MIN and AUTO: plausible, but not enough to pick silently.
        let scan = DirScan::from_parts("/p", ["boot.py"], [], []);
        let confidence = confidence_for(&scan, BackendKind::MicroPython);
        assert!(
            (MIN_CONFIDENCE..AUTO_CONFIDENCE).contains(&confidence),
            "fixture no longer lands between the thresholds: {confidence}"
        );
        assert_eq!(
            outcome_for(&scan),
            DetectionOutcome::Ambiguous(vec![BackendKind::MicroPython])
        );
    }

    #[test]
    fn scores_are_ordered_and_explainable() {
        let scan = DirScan::from_parts("/p", ["boot.py", "main.py"], [], []);
        let scores = score_directory(&registry(), &scan);

        assert_eq!(scores.len(), BackendKind::ALL.len());
        assert_eq!(scores[0].kind, BackendKind::MicroPython);
        assert!(scores[0].confidence >= scores[1].confidence);
        // Every point of the score is attributable to a named signal.
        let summed: f32 = scores[0].signals.iter().map(|s| s.weight).sum();
        assert!((summed - scores[0].score).abs() < f32::EPSILON);
        assert!(scores[0].signals.iter().any(|s| s.id == "boot.py"));
    }

    #[test]
    fn confidence_never_exceeds_one() {
        let scan = DirScan::from_parts(
            "/p",
            [
                "CMakeLists.txt",
                "prj.conf",
                "west.yml",
                "app.overlay",
                "Kconfig",
                "sample.yaml",
            ],
            [".west", "boards"],
            [("CMakeLists.txt", "find_package(Zephyr)")],
        );
        assert_eq!(confidence_for(&scan, BackendKind::Zephyr), 1.0);
    }

    #[test]
    fn manual_override_replaces_outcome_but_keeps_evidence() {
        let scan = DirScan::from_parts("/p", ["boot.py", "main.py"], [], []);
        let detection = Detection {
            root: PathBuf::from("/p"),
            outcome: classify(&score_directory(&registry(), &scan)),
            scores: score_directory(&registry(), &scan),
            source: DetectionSource::Automatic,
            searched: vec![PathBuf::from("/p")],
        };

        let overridden = detection.overridden_with(BackendKind::Zephyr);
        assert_eq!(overridden.backend(), Some(BackendKind::Zephyr));
        assert_eq!(overridden.source, DetectionSource::Manual);
        // The heuristics' opinion survives for display.
        assert!(overridden.confidence_of(BackendKind::MicroPython) > 0.0);
    }
}
