# Model catalogs and account balances

## Kilo Gateway

The engine keeps Kilo's full advertised inventory and checks
[the CLI's gateway balance endpoint](https://github.com/Kilo-Org/kilo/blob/main/packages/kilo-gateway/src/api/profile.ts)
(`GET /api/profile/balance`) when serving the catalog:

| Account state | Gateway models shown |
| --- | --- |
| Positive balance | All advertised models |
| Zero or negative balance | Only explicitly free models |
| Anonymous gateway account | Only explicitly free models |
| Unknown balance, failed probe, or custom gateway endpoint | Preserve advertised models; do not assume zero |

The check uses Kilo's own saved API/OAuth token or `KILO_API_KEY`, and honors
the selected organization (`accountId`, `KILO_ORG_ID`, or provider options).
Global JSON/JSONC configuration and explicit `KILO_CONFIG`, `KILO_CONFIG_DIR`
and `KILO_CONFIG_CONTENT` overrides can also supply the token and organization.
Custom-endpoint credentials are not sent to Kilo's balance API.

Filtering happens **after** live, disk-cached and static inventory loading, so
balance changes do not require another login or destroy the paid inventory.
ChatGPT, Copilot and other independently authenticated providers inside Kilo
are not charged against the gateway balance and remain in the list. Desktop
and mobile clients preserve the host's decision instead of applying another
unconditional free-only filter.

If no free model is available at a zero balance, an empty catalog is intentional.
It must not be replaced by paid rows, including a previously funded mobile
catalog. A successful empty response replaces the mobile disk cache.

## Provider compatibility

Only providers supported by the current build can be configured or offered in
model pickers. Unsupported provider IDs from older disk caches or other devices
are ignored without losing the remaining providers, accounts, or warnings.
They never receive another provider's static models as a fallback.

Saved opt-outs, update policies, composer defaults, favorites and legacy
allow-lists retain the supported choices. An
unsupported title provider also clears its pinned model, so that model is not
silently reused with a different provider. Existing chat rows and transcripts
are preserved; choose a supported provider to continue a retired-provider chat.
Saved credentials and user history are not deleted by this compatibility path.

## Validation

Regression tests cover funded, empty and unknown Kilo balances, independent
upstream providers, authoritative empty catalogs, unsupported-provider cache
entries, preserved preferences and peer account/provider lists. Normal workspace
CI also covers provider installation/update matching and desktop/mobile builds.
