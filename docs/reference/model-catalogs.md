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
families are excluded from the chat picker. Declared tasks, chat capabilities,
supported endpoints and explicit availability flags also filter out models for
other APIs. Unknown/new chat models do not have to appear in a static allow-list.
Inventory membership still does **not** prove that NVIDIA's backing deployment
is live or accessible to the selected key; Zeron does not send inference prompts
just to test the catalog.

NIM now requires a key and uses **inventory only**: public featured metadata can
enrich an inventory row, but cannot create an offered ID. Built-in NIM model
suggestions have been removed. An empty inventory stays empty; a failed refresh
shows an error instead of falling back to featured, static or previous rows.
The harness keeps only a short-lived, key/endpoint-scoped memory inventory and
clears it on refresh failure. The engine ignores legacy NIM disk catalogs, and
mobile retires old NIM disk rows rather than using them offline. Other providers'
fallback behavior is unchanged.

Desktop clears NIM rows while refreshing and on refresh failure. Saved selections
absent from NIM's current inventory are not synthesized as picker choices. Desktop
and mobile preserve NIM wire IDs exactly: CLI-specific `default`/`-1m`/`[1m]`
alias normalization must not turn a real provider ID into a different model.
An omitted model is chosen from the actual request key's inventory, never a
hardcoded default. Update both the app and the device running chats for these
rules to apply; no manual cache-file deletion is needed.

### Known unavailable public-hosted entries

`writer/palmyra-fin-70b-32k` is excluded from Zeron's picker and implicit defaults
for NVIDIA's public hosted endpoint **before** a failed prompt, even if
`/v1/models` still lists it.
On 2026-10-28 its [NVIDIA model page](https://build.nvidia.com/writer/palmyra-fin-70b-32k)
returned a 404 page, matching the reported function/account-not-found inference
failure. This is a dated public-route exclusion, not a claim that the model's
weights or every private deployment have been retired. Other Writer IDs and
self-hosted/custom NIM endpoints are not excluded by this rule.

The exclusion is applied after memory-cache loading and does not expire when a
15-minute rejection timer runs out. Explicit model requests remain allowed; a
successful nonempty completion can restore the entry **only for that endpoint
and key**, provided it is still in the inventory. This positive evidence survives
restart in the same private store. A later definitive model rejection revokes it,
including after the short negative-cache timer expires. Inventory metadata alone
cannot claim this proof; no test prompts are sent automatically.

An existing chat's selected model is not silently replaced. Choose a different
model in that chat rather than repeatedly sending to the old selection. The
error now says this instead of implying the hosted entry returns after a timer.

### Other model-specific failures

If an actual chat request returns NVIDIA's `404` / `Function …: Not found for
account …` response (or a structured `model_not_found` rejection), an entry not in
the known hosted exclusion set is hidden for **15 minutes**, scoped to the exact
endpoint and API key. This filter
runs after inventory loading (including the short-lived memory cache), including
when every row is rejected. Refreshing the inventory cannot immediately resurrect
the same broken model. Other models, accounts and endpoints are unaffected.

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

The prior model-specific 404 change passed GitHub core, engine, UI, app and
cross-platform checks. For the inventory-only and Palmyra follow-ups, **48 focused checks**
passed locally using the production client catalog, availability and discovery
logic; extracted production inventory/cache/error logic and type definitions;
and real Serde, JSON, Tokio, Tracing and Thiserror dependencies. The focused
client tests use a standard-library temporary-directory fixture. These checks
are not a full production-workspace build.

Changed Rust passed parser checks, offline workspace metadata validation and
`git diff --check`. Full local workspace tests remain blocked by the uncached
`gpui` Git dependency. New HTTP/default-model, engine-cache and desktop-picker
fixtures are included for normal CI. Live authenticated NVIDIA calls have not
been run in this sandbox.
