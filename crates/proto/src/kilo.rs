//! Kilo Code — provider knowledge shared by the engine, the viewer client,
//! and the desktop UI.
//!
//! Kilo advertises a gateway catalog alongside independently authenticated
//! providers. The engine filters gateway rows using the current account's
//! balance; clients must not apply an unconditional free-only filter, which
//! would hide funded models after the engine has already authorized them.

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

/// Apply a confirmed balance to a catalog. Positive or unknown balances keep
/// the full catalog. Zero/negative balances keep ONLY models available without
/// gateway credits (including independently authenticated upstream providers).
/// An empty result is intentional: never substitute paid rows when there is
/// no money just because no free rows matched.
///
/// `None` means the provider could not report a balance, NOT a zero balance.
/// An anonymous gateway account can be passed as `Some(0.0)`.
pub fn models_for_balance<T>(
    rows: Vec<T>,
    balance: Option<f64>,
    available_without_credits: impl Fn(&T) -> bool,
) -> Vec<T> {
    if balance.is_some_and(|balance| balance.is_finite() && balance <= 0.0) {
        rows.into_iter().filter(available_without_credits).collect()
    } else {
        rows
    }
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
    fn funded_and_unknown_balances_keep_all_models() {
        for balance in [Some(0.01), Some(10.0), None, Some(f64::NAN)] {
            assert_eq!(
                models_for_balance(vec!["free", "paid"], balance, |row| *row == "free"),
                vec!["free", "paid"]
            );
        }
    }

    #[test]
    fn empty_balances_never_fall_back_to_paid_models() {
        for balance in [0.0, -1.0] {
            assert_eq!(
                models_for_balance(vec!["free", "paid"], Some(balance), |row| *row == "free"),
                vec!["free"]
            );
            assert!(models_for_balance(vec!["paid"], Some(balance), |_| false).is_empty());
        }
    }

    #[test]
    fn gateway_balance_does_not_hide_independent_providers() {
        let kept = models_for_balance(
            vec!["kilo/router/free", "kilo/paid", "openai/gpt"],
            Some(0.0),
            |id| !id.starts_with("kilo/") || gateway_model_is_free(id, ""),
        );
        assert_eq!(kept, vec!["kilo/router/free", "openai/gpt"]);
    }
}
