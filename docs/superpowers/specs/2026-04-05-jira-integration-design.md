# JIRA Integration Design

## Overview

Add JIRA integration to hearken, enabling automatic creation and updating of JIRA tickets based on discovered log patterns. The integration supports both JIRA Cloud and JIRA Server/Data Center, and is delivered as a new workspace crate behind a cargo feature flag.

## Goals

- Create JIRA tickets for discovered patterns based on configurable filters
- Update existing tickets with current stats and change summaries
- Support both JIRA Cloud (API v3) and Server/Data Center (API v2)
- Keep integration fully optional — zero impact on users who don't need it
- No local sync state — JIRA is the single source of truth

## Non-Goals

- Bi-directional sync (e.g., closing a JIRA ticket suppresses a pattern)
- Custom field creation or JIRA admin operations
- JIRA OAuth2 flows — only token/PAT-based auth

---

## Crate Structure

New crate `hearken-jira/` added to the workspace, following the existing pattern (core, ml, storage, cli):

```
hearken-jira/
├── Cargo.toml
└── src/
    ├── lib.rs          # Public API: JiraClient, JiraConfig, sync/status operations
    ├── client.rs       # HTTP client: create/update/search tickets, Cloud vs Server auth
    ├── mapper.rs       # Pattern -> ticket mapping: build title/description, parse markers
    └── filter.rs       # Pattern filtering: anomalies, tags, thresholds, occurrence counts
```

### Dependencies

- `reqwest` (with `json` feature) — HTTP client
- `serde` / `serde_json` — serialization
- `hearken-core` — `LogTemplate` type
- `hearken-storage` — read patterns, occurrences, anomalies, tags from the database

### Feature Flag

In `hearken-cli/Cargo.toml`:

```toml
[features]
jira = ["hearken-jira"]

[dependencies]
hearken-jira = { path = "../hearken-jira", optional = true }
```

All JIRA-related CLI code in `hearken-cli` is gated behind `#[cfg(feature = "jira")]`.

---

## Configuration

### `.hearken.toml` section

```toml
[jira]
url = "https://mycompany.atlassian.net"
project = "OPS"
label = "hearken"
type = "cloud"        # "cloud" or "server"
issue_type = "Bug"    # optional, defaults to "Bug"
```

### Environment variables

- `HEARKEN_JIRA_USER` — email (Cloud) or username (Server)
- `HEARKEN_JIRA_TOKEN` — API token (Cloud) or PAT (Server)

### Config struct

```rust
pub struct JiraConfig {
    pub url: String,
    pub project: String,
    pub label: String,
    pub instance_type: JiraInstanceType,
    pub issue_type: String,  // defaults to "Bug"
    pub user: String,        // from env
    pub token: String,       // from env
}

pub enum JiraInstanceType {
    Cloud,
    Server,
}
```

### Validation

JIRA subcommands fail early with a clear error if:
- The `[jira]` config section is missing
- `HEARKEN_JIRA_USER` or `HEARKEN_JIRA_TOKEN` are not set
- `type` is not `"cloud"` or `"server"`

---

## CLI Commands

### Subcommand structure

```
hearken jira status     Show sync state: pattern counts, ticket counts, connection check
hearken jira sync       Create new tickets + update existing ones
hearken jira update     Only update existing tickets (no new ticket creation)
```

### Filter flags (shared by `sync` and `update`)

```
--anomalies-only           Only patterns flagged as anomalies
--tags <tag1,tag2>         Only patterns with these tags
--exclude-tags <t1,t2>     Exclude patterns with these tags
--min-occurrences <N>      Only patterns with >= N occurrences
--new-only                 Only patterns not yet synced to JIRA (sync only)
--dry-run                  Show what would happen without making API calls
```

### Inline integration (on `process` and `watch`)

```
--jira-sync                After processing, run the equivalent of `jira sync`
```

The `--jira-sync` flag triggers the same sync logic at the end of a processing batch. All filtering in this mode comes from config defaults or additional flags.

### `jira status` output

```
JIRA Sync Status (project: OPS, label: hearken)
  Total patterns:     847
  With JIRA tickets:  123
  New (unsynced):      34
  Changed since sync:  12
  JIRA connection:     OK
```

---

## Pattern-to-Ticket Mapping

### Ticket creation

When creating a new JIRA ticket for a pattern:

- **Summary (title):** `[hearken] <truncated pattern template>` — JIRA limits summary to 255 characters
- **Description:** Structured body containing:
  - Pattern template (full, untruncated)
  - Occurrence count
  - First seen / last seen timestamps
  - Source file group
  - Sample log lines (configurable, default 5)
  - Embedded marker in a code block at the bottom of the description (see below)
- **Labels:** The configured label (e.g., `hearken`)
- **Issue type:** From config, defaults to `Bug`

### Marker format

The marker is embedded as a code block at the bottom of every hearken-managed ticket description. This format survives round-trips in both JIRA Cloud (ADF) and Server (wiki markup), since code blocks are preserved verbatim by both rendering pipelines.

```
{code:title=hearken-metadata}
hearken:db=myproject.db;pattern_id=42;occurrences=1892
{code}
```

In ADF (Cloud), this becomes a `codeBlock` node. In wiki markup (Server), it's a `{code}` macro. Both are parseable when read back via the API.

The `occurrences` field in the marker is used to detect changes without local state (see "Determining changed since sync" below).

### Matching existing tickets

On every sync/update/status, hearken queries JIRA:

```
JQL: project = "OPS" AND labels = "hearken"
```

Then parses the code block marker from each ticket's description. This builds a `HashMap<(String, i64), JiraTicket>` mapping (db name, pattern ID) pairs to existing tickets.

The `db` component in the marker prevents pattern ID collisions when multiple hearken databases sync to the same JIRA project.

### Updating existing tickets

1. **Description** — regenerated with current stats (occurrence count, timestamps, samples, updated marker)
2. **Comment** — added with a change summary:
   ```
   [hearken sync] Updated 2026-04-05T14:30:00Z
   - Occurrences: 1,204 -> 1,892 (+688)
   - Last seen: 2026-04-05T14:28:33Z
   - New sample lines added
   ```

### Determining "changed since sync"

Since there's no local state, hearken parses the `occurrences` field from the code block marker in the JIRA ticket description and compares it to the current count in the database. If they differ, the pattern is considered changed.

---

## JIRA API Interaction

### Endpoints

| Operation          | Cloud                              | Server                             |
|--------------------|------------------------------------|------------------------------------|
| Search tickets     | `POST /rest/api/3/search`          | `POST /rest/api/2/search`          |
| Create ticket      | `POST /rest/api/3/issue`           | `POST /rest/api/2/issue`           |
| Update description | `PUT /rest/api/3/issue/{key}`      | `PUT /rest/api/2/issue/{key}`      |
| Add comment        | `POST /rest/api/3/issue/{key}/comment` | `POST /rest/api/2/issue/{key}/comment` |

### Cloud vs Server differences

- **Cloud API v3** uses Atlassian Document Format (ADF) for description and comments — structured JSON
- **Server API v2** uses wiki markup or plain text

The `client.rs` module abstracts this behind a `JiraClient` that dispatches to the correct API version and body format based on `JiraInstanceType`. The `mapper.rs` module generates both ADF and wiki markup representations of the same content.

### Authentication

- **Cloud:** Basic auth — `Authorization: Basic base64(user:token)`
- **Server:** Bearer token — `Authorization: Bearer <token>`, with fallback to basic auth

### Pagination

JIRA search returns max 50 results by default. The client handles pagination transparently, fetching all matching tickets before proceeding with sync.

### Rate limiting

JIRA Cloud has rate limits. The client respects `Retry-After` headers and backs off on 429 responses. Ticket creation/updates are sequential — no aggressive parallelism needed given expected volumes.

### Error handling

- Non-200 responses are surfaced with the JIRA error message
- A single failed ticket does not abort the entire sync — errors are collected and reported at the end
- A summary is printed: `Synced 45 tickets (3 created, 40 updated, 2 failed)`

---

## Data Flow

### `hearken jira sync` flow

```
1. Load config (.hearken.toml [jira] section + env vars)
2. Validate config, fail early if incomplete
3. Query JIRA: JQL search for all tickets with configured label
4. Parse markers from ticket descriptions -> HashMap<(db, pattern_id), JiraTicket>
5. Load patterns from hearken database (apply filters)
6. For each pattern:
   a. If existing ticket found -> update description + add comment (if changed)
   b. If no ticket found -> create new ticket
7. Print summary: created / updated / unchanged / failed
```

### `hearken jira update` flow

Same as sync, but step 6b is skipped — only existing tickets are updated.

### `hearken jira status` flow

```
1. Load config, validate
2. Query JIRA for tickets with label
3. Parse markers -> build mapping
4. Load pattern count from database
5. Compare and report: total patterns, with tickets, new, changed
```

### Inline (`process --jira-sync`) flow

```
1. Normal process pipeline runs
2. After processing completes, if --jira-sync flag is set:
   a. Load JIRA config
   b. Run the sync flow (same as `hearken jira sync`)
```
