# Model catalogs and account balances

## NVIDIA NIM

Connect an `nvapi-…` key in **Settings → Providers → NVIDIA NIM**, then refresh
its model list. Discovery and inference use the selected account, with
`NVIDIA_API_KEY` as the device-level fallback. Keys are never exported into the
process environment or sent to NVIDIA's public featured-metadata host.

The picker reads the complete `/v1/models` inventory, not just the featured
models. Featured metadata supplies labels and context limits when available;
a missing featured feed does not hide other inventory models. Explicit non-chat
tasks and known embedding, reranking, reward, speech and image-generation
families are excluded from the chat picker. Inventory membership does **not**
prove that NVIDIA's backing deployment is live or accessible to the selected key;
Zeron does not send inference prompts just to test the catalog.

If an actual chat request returns NVIDIA's `404` / `Function …: Not found for
account …` response (or a structured `model_not_found` rejection), that model is
hidden for **15 minutes**, scoped to the exact endpoint and API key. This filter
runs after live, memory, disk and static catalog selection, including when every
row is rejected. Refreshing the inventory cannot immediately resurrect the same
broken model. Other models, accounts and endpoints are unaffected.

The short-lived evidence survives an app restart in private files containing
only model IDs and timestamps under a hashed endpoint/key namespace. It expires
automatically; an explicit retry is still allowed, and a successful response
restores the model immediately. Zeron never silently substitutes a different
model. Generic route `404`s, authentication failures, throttling and server or
network failures do not hide catalog rows. NVIDIA controls deployment/access;
this error is **not** evidence of a zero balance or an invalid key.

`NVIDIA_NIM_BASE_URL` accepts either the API origin or an OpenAI-style base URL
ending in `/v1`; Zeron does not append `/v1` twice. Catalog caches are scoped to
the endpoint and selected key, and a key change during discovery discards the
old response. Authentication failures are not disguised as a public/static
catalog.

NVIDIA's inventory does **not** report model prices or account balances. Zeron
therefore does not claim these models are free or infer a zero balance from an
API key. A payment-required response explains that inference credits must be
checked with NVIDIA.

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

## Validation

Regression tests cover full NIM inventory discovery, optional featured metadata,
selected-key authentication, account-switch races, `/v1` base URLs, streaming
conversation resumption, balance changes in both directions, team headers,
unknown balances, independent upstream providers, and authoritative empty
catalogs.

The 404 follow-up adds regressions for exact error classification, account and
endpoint isolation, request-key overrides, durable expiry/recovery, private
storage, and authoritative empty results across live, disk and static sources.
The HTTP fixtures assert that discovery does not send inference prompts and
that a failed model is never silently replaced.

In the restricted development sandbox, 23 focused checks passed using the
production availability store, extracted production error helpers/tests, the
existing redaction/free-model policy modules, and real Serde/JSON dependencies.
These are not a full production-crate build. Changed Rust files passed parser
checks, workspace manifests and the lockfile passed offline metadata validation,
and `git diff --check` was clean. The full workspace test command could not run
because required Git/registry dependencies were not locally cached. Async HTTP
and engine-cache fixture tests and live authenticated provider calls still need
to run in the normal build environment.
