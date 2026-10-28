# Model catalogs and account balances

## NVIDIA NIM

Connect an `nvapi-…` key in **Settings → Providers → NVIDIA NIM**, then refresh
its model list. Discovery and inference use the selected account, with
`NVIDIA_API_KEY` as the device-level fallback. Keys are never exported into the
process environment or sent to NVIDIA's public featured-metadata host.

### Free hosted endpoints only

For NVIDIA's public endpoint, the picker now offers only the intersection of:

1. The selected key's complete `/v1/models` inventory.
2. NVIDIA's current **Free Endpoint** catalog, fetched without credentials from
   `https://api.ngc.nvidia.com/v2/search/catalog/resources/ENDPOINT` using
   `nimType=nim_type_preview`, with every result page checked.
3. A public deployment spec declaring an NVIDIA-hosted chat-completions route,
   a deployment function, and no unavailable banner.
4. Models not rejected by actual requests for this endpoint/key.

This is the designation behind NVIDIA's [Free Endpoint filter](https://build.nvidia.com/models?filters=nimType%3Anim_type_preview).
A downloadable model, a paid partner endpoint, a featured recommendation, or a
`(free)` display name is **not** enough. Each record must carry the actual free
mark. NGC's `AVAILABLE` attribute describes download availability, not hosted
inference, and does not disqualify a hosted-only free endpoint.

Wire IDs are intersected exactly with the authenticated inventory. The public
spec's example/default/enum IDs handle card slugs that differ from API IDs;
Zeron does not guess aliases, change punctuation in a request, use an NGC org as
a model publisher, or add models absent from the inventory. A spec without
explicit model IDs can only match the literal publisher/card-name ID. Featured
metadata remains optional label/context enrichment, never an allow-list or
pricing authority. New free models do not need a built-in ID entry.

Public free discovery and deployment checks are **GET metadata requests only**;
no inference prompts, credit-consuming probes, or paid fallback requests are
sent. NVIDIA describes these endpoints as free for development/prototyping,
subject to its terms and rate limits, not as an unlimited production service.
A free badge and public deployment metadata still cannot guarantee entitlement
or uptime for every account; only an actual request can establish that.

Free-catalog or deployment-metadata failures show an error instead of offering
unverified inventory, old rows, or a hardcoded list. Successful empty results
stay empty. Public descriptions identify the free hosted endpoint and its rate
limits. **The same check applies before inference**, including a saved chat's
explicit model and title requests: a model not verified by the current free
catalog is rejected before any inference POST. A different model is never
silently substituted.

### Caches, accounts, and custom endpoints

NIM keeps only a short-lived, key/endpoint/policy-scoped memory catalog and
clears it on refresh failure. The engine ignores legacy NIM disk catalogs;
mobile retires old NIM disk rows instead of offering them offline. Built-in NIM
model suggestions remain removed. Desktop clears NIM rows while refreshing and
on failure, and does not synthesize missing saved selections as choices.
Desktop and mobile preserve provider wire IDs exactly. The policy/context
version is now `openai-chat-completions-free-v4` to invalidate earlier inventories.

An omitted model is chosen from the actual request key's eligible catalog.
An existing chat's selected model is not silently replaced: choose a free model
in that chat. Update both the app and the device running chats for these rules
to apply; no manual cache-file deletion is needed.

`NVIDIA_NIM_BASE_URL` accepts an origin or an OpenAI-style base ending in `/v1`.
The public free designation applies only to NVIDIA's public HTTPS endpoint,
including its explicit default port. Custom/self-hosted NIMs retain their own
inventory, prices and access rules; NVIDIA's public free catalog cannot certify
them as free. They are not annotated as free or restricted by the hosted catalog.
Other providers' catalog/balance policies are unchanged.

### Account-specific failures and recovery

A definitive model-specific `404` (`Function …: Not found for account …`,
`model_not_found`, etc.) or structured model-retirement `410` excludes the model
from public hosted choices and implicit defaults **until a successful explicit
retry**, not just for 15 minutes. A metadata refresh or app restart cannot
resurrect that failure. Explicit retries are allowed only while the model still
qualifies as a free hosted chat endpoint; successful retry restores that model
for this key without making another key or endpoint eligible.

The evidence is private, atomic, key/endpoint-scoped storage containing only
model IDs and timestamps under a hashed namespace. Custom endpoints keep the
previous 15-minute negative-cache policy. Generic route errors, authentication,
billing, throttling, network problems and server failures are not model-specific
rejection evidence. An inference-access denial is not automatically an invalid
key or an empty balance: check Public API Endpoints permissions and trial limits.
NVIDIA's [staff confirmation](https://forums.developer.nvidia.com/t/request-more-4-000-credits-option-on-build-nvidia-com/344567/2)
says the hosted credit system was replaced by model-specific trial rate limits.
A payment-required response does not enable any paid fallback.

`writer/palmyra-fin-70b-32k` additionally retains the dated public-route exclusion:
on 2026-10-28 its [NVIDIA model page](https://build.nvidia.com/writer/palmyra-fin-70b-32k)
rendered a 404 page, matching the reported inference rejection. This is not a
claim that its weights or every private deployment are retired. A nonempty
successful explicit retry can override that dated exclusion only for its key,
and only if the model now also passes the live free-hosted policy. Positive
proof survives restart and a later definitive rejection revokes it. Other
Writer IDs and self-hosted deployments are not banned.

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
cross-platform checks. For the free-hosted, inventory-only and Palmyra follow-ups,
**62 focused checks** passed locally using the production client catalog,
availability, discovery and free-hosted metadata policy; extracted production
inventory/cache/error logic and type definitions;
and real Serde, JSON, Tokio, Tracing and Thiserror dependencies. The focused
client tests use a standard-library temporary-directory fixture. These checks
are not a full production-workspace build.

Changed Rust passed parser checks, offline workspace metadata validation and
`git diff --check`. Full local workspace tests remain blocked by the uncached
`gpui` Git dependency. New free-catalog HTTP/no-paid-POST/default-model,
engine-cache and desktop-picker fixtures are included for normal CI. Live authenticated NVIDIA calls have not
been run in this sandbox.
