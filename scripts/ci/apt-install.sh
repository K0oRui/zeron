#!/usr/bin/env bash
# Install apt packages from a user-owned, cacheable .deb directory with
# stall-resistant retries and per-phase timestamps.
#   usage: APT_DEB_DIR=~/apt-debs [APT_RECOMMENDS=1] apt-install.sh pkg...
# Hosted-runner apt mirrors occasionally stall for minutes (observed 7-14 min
# for a ~400 MB install); the bounded download retries below, the fallback to
# the public archive and the actions/cache of the .debs make the common case
# ~20 s and the bad case bounded.
set -uo pipefail
dir=${APT_DEB_DIR:-$HOME/apt-debs}
primary=${APT_SOURCES_LIST:-/etc/apt/sources.list.d/ubuntu.sources}
mkdir -p "$dir/partial"
export DEBIAN_FRONTEND=noninteractive
ts() { while IFS= read -r l; do printf '[%(%T)T] %s\n' -1 "$l"; done; }
log() { printf '[%(%T)T] apt-install: %s\n' -1 "$*"; }

# No docs/man/locales, no fsync: faster unpack.
printf '%s\n' force-unsafe-io 'path-exclude=/usr/share/doc/*' 'path-exclude=/usr/share/man/*' \
  'path-exclude=/usr/share/locale/*' 'path-exclude=/usr/share/info/*' | sudo tee /etc/dpkg/dpkg.cfg.d/90ci >/dev/null
sudo rm -f /var/lib/man-db/auto-update

APT=(-y -o Acquire::Retries=5 -o Acquire::http::Timeout=15 -o Acquire::Languages=none
     -o "Dir::Cache::archives=$dir" -o APT::Sandbox::User=root -o DPkg::Lock::Timeout=120)
[ "${APT_RECOMMENDS:-0}" = 1 ] || APT+=(--no-install-recommends)

packages=("$@")
log "cached debs: $(ls "$dir"/*.deb 2>/dev/null | wc -l)"

# A second host for the same suites. The runner image can point apt at a mirror
# LIST (`mirror+file:/etc/apt/apt-mirrors.txt`, which resolves to one regional
# mirror), so rewriting a hostname in the sources file would not move apt at
# all; rebuild a single stanza from the file's own Types/Suites/Components and
# send it to the public archive instead — a different host and CDN, so a
# degraded runner mirror cannot wedge the job.
# Every value of one deb822 field across the file's stanzas, de-duplicated.
field_words() { sed -n "s/^$1:[[:space:]]*//p" "$2" | tr ' ' '\n' | awk 'NF && !seen[$0]++'; }
alt_sources() {
  local src=$1 dest=$2 suites components signed
  suites=$(field_words Suites "$src" | paste -sd' ' -)
  components=$(field_words Components "$src" | paste -sd' ' -)
  signed=$(field_words Signed-By "$src" | paste -sd', ' -)
  {
    # Binary packages only: the suites and components are the file's own, so a
    # two-stanza file (release + security) keeps both pockets on one host.
    printf 'Types: deb\n'
    printf 'URIs: http://archive.ubuntu.com/ubuntu/\n'
    printf 'Suites: %s\n' "${suites:-noble noble-updates noble-security}"
    printf 'Components: %s\n' "${components:-main restricted universe multiverse}"
    if [ -n "$signed" ]; then printf 'Signed-By: %s\n' "$signed"; fi
  } >"$dest"
}

# Only Ubuntu packages are needed: unrelated runner repositories can be out of
# sync, and the fallback stanza must not be diluted by them either.
update_index() { # $1 = sources file ('' = the runner's own ubuntu.sources)
  if [ -n "${1:-}" ]; then
    log "update ($1)"
    sudo timeout 120 apt-get update -qq -o Acquire::Retries=3 -o Acquire::http::Timeout=15 \
      -o Dir::Etc::sourcelist="$1" -o Dir::Etc::sourceparts=- 2>&1 | ts
  else
    log "update"
    sudo timeout 120 apt-get update -qq -o Acquire::Retries=3 -o Acquire::http::Timeout=15 \
      -o Dir::Etc::sourcelist="sources.list.d/ubuntu.sources" -o Dir::Etc::sourceparts=- 2>&1 | ts
  fi
}

# Download only: apt keeps partial .debs in $dir/partial, so each attempt
# resumes rather than restarts. $1 is the sources file ('' = runner's own),
# the rest are the per-attempt time bounds.
download() {
  local src=$1
  shift
  local -a opt=()
  local bound
  if [ -n "$src" ]; then opt=(-o "Dir::Etc::sourcelist=$src" -o Dir::Etc::sourceparts=-); fi
  for bound in "$@"; do
    log "download attempt ${bound}s${src:+ via $src}"
    sudo timeout "$bound" apt-get install "${APT[@]}" "${opt[@]}" \
      --download-only "${packages[@]}" 2>&1 | ts
    if [ "${PIPESTATUS[0]}" = 0 ]; then return 0; fi
  done
  return 1
}

used=''
if update_index '' && download '' 150 150 150; then
  log "downloaded from the runner mirror"
else
  log "the runner mirror kept stalling; retrying via archive.ubuntu.com"
  alt=$(mktemp "${TMPDIR:-/tmp}/ubuntu-archive.XXXXXX.sources")
  if alt_sources "$primary" "$alt" && update_index "$alt" && download "$alt" 210 210; then
    used=$alt
    log "downloaded from archive.ubuntu.com"
  else
    log "download failed"
    exit 1
  fi
fi

log "install"
if [ -n "$used" ]; then
  sudo apt-get install "${APT[@]}" -o "Dir::Etc::sourcelist=$used" -o Dir::Etc::sourceparts=- \
    -qq "${packages[@]}" 2>&1 | ts
else
  sudo apt-get install "${APT[@]}" -qq "${packages[@]}" 2>&1 | ts
fi
rc=${PIPESTATUS[0]}
log "done rc=$rc"
exit "$rc"
