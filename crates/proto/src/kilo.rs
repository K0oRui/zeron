//! Kilo Code — provider knowledge shared by the engine, the viewer client,
//! and the desktop UI.
//!
//! The Kilo CLI (an opencode fork driven over its own ACP server, `kilo acp`)
//! advertises its WHOLE gateway catalog on session setup: hundreds of model
//! rows, most of which need paid credits, each labelled with the gateway's
//! provider prefix. Zeron's picker keeps only the rows an account can run
//! without buying credits and drops the redundant prefix. The rule lives here
//! exactly once: the harness's discovery-side filter, the relay client's
//! `normalize_models`, and the desktop picker's `normalize_model_rows` all
//! apply it identically (each previously carried its own copy, free to drift).

/// Label prefix on every Kilo gateway row (`Kilo Gateway/Auto Free`). The
/// picker already sits under the Kilo harness, so the prefix is noise.
pub const GATEWAY_LABEL_PREFIX: &str = "Kilo Gateway/";

/// Whether a gateway row is runnable without buying credits. Kilo's free
/// rows are the `.../free` routers (`kilo/kilo-auto/free` — its default
/// anonymous model) plus the OpenRouter-style `*:free` models; gateway labels
/// also carry a trailing "(free)" on those rows ("Kilo Gateway/Auto Free"
/// included) and the router rows read "... Auto Free" / "... Free Models
/// Router". Id and label are both matched case-insensitively.
pub fn gateway_model_is_free(id: &str, label: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let label = label.to_ascii_lowercase();
    id.ends_with(":free")
        || id.ends_with("-free")
        || id.ends_with("/free")
        || label.ends_with(" (free)")
        || label.ends_with("auto free")
        || label.ends_with("free models router")
}

/// The gateway label without its provider prefix (see
/// [`GATEWAY_LABEL_PREFIX`]); a legacy row saved by an engine that never
/// stripped it still displays clean.
pub fn strip_gateway_label(label: &str) -> &str {
    label.strip_prefix(GATEWAY_LABEL_PREFIX).unwrap_or(label)
}

/// Keep only the free gateway rows — or, when the rule matches nothing (the
/// gateway renamed its free tier), keep the list as-is rather than showing an
/// empty picker. `is_free` is normally [`gateway_model_is_free`] adapted to
/// the caller's model type:
///
/// ```
/// struct Row { id: String, label: String }
/// let rows = vec![
///     Row { id: "kilo/kilo-auto/free".into(), label: "Kilo Gateway/Auto Free".into() },
///     Row { id: "kilo/anthropic/claude-sonnet-5".into(), label: "Kilo Gateway/Claude Sonnet 5".into() },
/// ];
/// let kept = zeron_proto::kilo::keep_free_or_all(rows, |row| {
///     zeron_proto::kilo::gateway_model_is_free(&row.id, &row.label)
/// });
/// assert_eq!(kept.len(), 1);
/// assert_eq!(kept[0].id, "kilo/kilo-auto/free");
/// ```
pub fn keep_free_or_all<T>(rows: Vec<T>, is_free: impl Fn(&T) -> bool) -> Vec<T> {
    let mut free = Vec::new();
    let mut paid = Vec::new();
    for row in rows {
        if is_free(&row) {
            free.push(row);
        } else {
            paid.push(row);
        }
    }
    if free.is_empty() { paid } else { free }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_rows_match_by_id_suffix_or_label_suffix() {
        // Kilo's anonymous default router.
        assert!(gateway_model_is_free("kilo/kilo-auto/free", "Kilo Gateway/Auto Free"));
        // OpenRouter-style free models.
        assert!(gateway_model_is_free(
            "kilo/nvidia/nemotron-3-super-120b-a12b:free",
            "Kilo Gateway/NVIDIA: Nemotron 3 Super (free)",
        ));
        assert!(gateway_model_is_free("kilo/minimax/m2.1-free", "Minimax M2.1"));
        // Label-only matches ("... (free)", "Auto Free", "Free Models Router").
        assert!(gateway_model_is_free("kilo/x/y", "Kilo Gateway/Y (free)"));
        assert!(gateway_model_is_free("kilo/auto", "KILO GATEWAY/AUTO FREE"));
        assert!(gateway_model_is_free("kilo/router", "OpenRouter Free Models Router"));
        // Paid rows stay out; a name merely CONTAINING "free" is not free.
        assert!(!gateway_model_is_free(
            "kilo/anthropic/claude-sonnet-5",
            "Kilo Gateway/Anthropic: Claude Sonnet 5",
        ));
        assert!(!gateway_model_is_free("kilo/freeform/model", "Kilo Gateway/Freeform"));
        assert!(!gateway_model_is_free("kilo/x", ""));
    }

    #[test]
    fn strip_drops_only_the_gateway_prefix() {
        assert_eq!(strip_gateway_label("Kilo Gateway/Auto Free"), "Auto Free");
        assert_eq!(
            strip_gateway_label("Kilo Gateway/NVIDIA: Nemotron 3 Super (free)"),
            "NVIDIA: Nemotron 3 Super (free)"
        );
        assert_eq!(strip_gateway_label("Auto Free"), "Auto Free");
        assert_eq!(strip_gateway_label(""), "");
    }

    #[test]
    fn keep_free_or_all_never_empties_a_non_empty_list() {
        struct Row {
            id: &'static str,
            label: &'static str,
        }
        let rows = || {
            vec![
                Row { id: "kilo/kilo-auto/free", label: "Kilo Gateway/Auto Free" },
                Row { id: "kilo/anthropic/claude-sonnet-5", label: "Kilo Gateway/Claude Sonnet 5" },
            ]
        };
        let kept =
            keep_free_or_all(rows(), |row| gateway_model_is_free(row.id, row.label));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "kilo/kilo-auto/free");

        // No free row matches (a renamed free tier): the paid list is shown
        // whole rather than an empty picker.
        let paid_only = keep_free_or_all(
            vec![Row { id: "kilo/anthropic/claude-sonnet-5", label: "Kilo Gateway/Claude Sonnet 5" }],
            |row| gateway_model_is_free(row.id, row.label),
        );
        assert_eq!(paid_only.len(), 1);
    }
}
