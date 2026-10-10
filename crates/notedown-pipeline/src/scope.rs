//! Which notes the pipeline ingests (spec 4.5).

use crate::config::{PipelineConfig, glob_match};

/// Whether the note at wire `path` with the device `keywords` is in scope: it matches one of
/// `ingest_folders`, none of `ingest_exclude`, and, when `ingest_keyword` is set, carries that
/// keyword. Keywords compare trimmed and without regard to case.
pub fn in_scope(cfg: &PipelineConfig, path: &str, keywords: &[String]) -> bool {
    let folder = cfg.ingest_folders.iter().any(|p| glob_match(p, path));
    let excluded = cfg.ingest_exclude.iter().any(|p| glob_match(p, path));
    let keyword = match &cfg.ingest_keyword {
        None => true,
        Some(k) => {
            let k = k.trim().to_lowercase();
            keywords.iter().any(|w| w.trim().to_lowercase() == k)
        }
    };
    folder && !excluded && keyword
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_exclude_and_keyword() {
        let mut cfg = PipelineConfig::default();
        assert!(in_scope(&cfg, "NOTE/Note/a.note", &[]));
        assert!(in_scope(&cfg, "NOTE/Note/x/a.note", &[]));
        assert!(!in_scope(&cfg, "DOCUMENT/a.note", &[]));
        cfg.ingest_exclude = vec!["NOTE/Note/private/**".into()];
        assert!(!in_scope(&cfg, "NOTE/Note/private/a.note", &[]));
        assert!(in_scope(&cfg, "NOTE/Note/a.note", &[]));
        cfg.ingest_keyword = Some("AI".into());
        assert!(!in_scope(&cfg, "NOTE/Note/a.note", &[]));
        assert!(!in_scope(&cfg, "NOTE/Note/a.note", &["aim".into()]));
        assert!(in_scope(
            &cfg,
            "NOTE/Note/a.note",
            &["work".into(), " ai ".into()]
        ));
        assert!(!in_scope(&cfg, "NOTE/Note/private/a.note", &["ai".into()]));
    }
}
