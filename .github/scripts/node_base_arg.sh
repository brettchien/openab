#!/usr/bin/env bash
# Single parser for Node.js base images in Dockerfiles. CI uses it both to
# decide which images need a runtime `node --version` check and to enforce that
# every Node.js base is pinned as node:${NODE<major>_VERSION}-<suffix>.
#
# Usage:
#   node_base_arg.sh <dockerfile> [target]
#       Print the NODE<n>_VERSION ARG that <target> (default: the last stage)
#       is ultimately built FROM, or nothing if it is not Node-based.
#   node_base_arg.sh --check <dockerfile>...
#       Validate every stage of every file and print "<file> NODE<n>_VERSION"
#       once per pin each file uses.
#
# Both modes scan every FROM in the file and exit 1 if any stage:
#   - references a Node.js image (any registry or namespace whose repository is
#     "node", e.g. node:, node@, library/node, mirror.gcr.io/library/node)
#     not written as node:${NODE<n>_VERSION}-<suffix>; or
#   - takes its base from a build arg (FROM ${VAR}) other than an allowlisted one,
#     or uses any build arg other than ${NODE<n>_VERSION} inside the reference
#     (e.g. node${TAG}-slim, ${REG}/node:...), since that can hide an unpinned
#     Node.js base.
# FROM lines continued with "\" are joined first, as Docker does; a FROM with no
# image, or an "# escape=" parser directive, is rejected.
# Known limitation: a line starting with FROM inside a RUN heredoc is parsed as
# an instruction. That can only add a false error, never hide a Node.js base.
# The first mode also fails if <target> is not a stage in the file.
# Stage names are matched case-insensitively, as Docker does.
set -euo pipefail

# Build args that may be used directly as a base image. BUILDER_IMAGE selects
# the openab builder in Dockerfile.unified and is never a Node.js runtime base.
# CI can override it, so only its declared ARG default is checked.
ALLOWED_VAR_BASES="BUILDER_IMAGE"

parse() { # parse <mode> <dockerfile> [target]
  awk -v mode="$1" -v target="${3:-}" -v allowed=" $ALLOWED_VAR_BASES " '
    # Classify one FROM image reference. Returns "node <ARG>", "none", or
    # "error <message>". Stage references inherit the referenced stage.
    function classify(img, depth,   key, ref, var, probe) {
      key = tolower(img)
      if (key in stage) return stage[key]
      if (img ~ /^[$][{]?[A-Za-z_][A-Za-z0-9_]*[}]?$/) {
        var = img; gsub(/[${}]/, "", var)
        if (index(allowed, " " var " ")) {
          # CI may override an allowed build arg, but its declared default must
          # not be a Node.js base either (e.g. ARG BUILDER_IMAGE=node:22).
          if (!(var in argdef) || argdef[var] == "") return "none"
          if (depth > 8) return "error " img " default refers back to itself"
          return classify(argdef[var], depth + 1)
        }
        return "error FROM " img " takes its base image from a build arg, which can hide an unpinned Node.js base; use node:${NODE<major>_VERSION}-<suffix> or a named stage (allowed build args:" allowed ")"
      }
      # Any other build arg in the reference (e.g. node${TAG}-slim, ${REG}/node:...)
      # cannot be resolved statically, so it could hide an unpinned Node.js base.
      probe = img; gsub(/[$][{]NODE[0-9]+_VERSION[}]/, "", probe)
      if (probe ~ /[$]/)
        return "error " img " uses a build arg that cannot be resolved statically; only ${NODE<major>_VERSION} may appear in a base image reference"
      ref = img
      sub(/^.*\//, "", ref)                                 # any registry/namespace: keep the repository name
      if (match(ref, /^node:[$][{]NODE[0-9]+_VERSION[}]-./))
        return "node " substr(ref, 8, index(ref, "}") - 8)   # text between "node:${" and "}"
      if (ref ~ /^node:[$][{]NODE[0-9]+_VERSION[}]$/)
        return "error " img " has no tag suffix; use node:${NODE<major>_VERSION}-<suffix> (e.g. -trixie-slim)"
      if (tolower(ref) ~ /^node([:@]|$)/)
        return "error " img " is not pinned; use node:${NODE<major>_VERSION}-<suffix>"
      return "none"
    }
    function report(lineno, msg) {
      print "::error file=" FILENAME ",line=" lineno "::" msg > "/dev/stderr"
      failed = 1
    }
    function from(line, lineno,   f, n, i, img, name, class, eq, v) {
      n = split(line, f)
      if (toupper(f[1]) == "ARG") {                    # remember ARG defaults for allowed build args
        for (i = 2; i <= n; i++) {
          eq = index(f[i], "=")
          if (eq == 0) continue
          v = substr(f[i], eq + 1); gsub(/^"|"$/, "", v)
          argdef[substr(f[i], 1, eq - 1)] = v
        }
        return
      }
      if (toupper(f[1]) != "FROM") return
      img = ""; name = ""
      for (i = 2; i <= n; i++) {
        if (f[i] ~ /^--/) continue                      # skip --platform=... and other flags
        if (img == "") { img = f[i]; continue }
        if (toupper(f[i]) == "AS" && i < n) { name = tolower(f[i + 1]); break }
      }
      if (img == "") { report(lineno, "FROM without an image reference"); return }
      class = classify(img, 0)
      if (class ~ /^error / && !(tolower(img) in stage))  # report each bad base once, not per child stage
        report(lineno, substr(class, 7))
      if (class ~ /^node /) used[substr(class, 6)] = 1
      if (name != "") stage[name] = class
      last = class
    }
    # A parser directive can change the line-continuation character; refuse it
    # rather than misread continued FROM lines.
    tolower($0) ~ /^#[[:space:]]*escape[[:space:]]*=/ {
      report(FNR, "the escape parser directive is not supported by node_base_arg.sh")
      next
    }
    /^[[:space:]]*#/ { next }                            # comments, also inside continuations
    {
      # Docker joins lines ending in "\" before parsing an instruction.
      line = $0
      if (buf == "") start = FNR
      if (line ~ /\\[[:space:]]*$/) { sub(/\\[[:space:]]*$/, " ", line); buf = buf line; next }
      from(buf line, start)
      buf = ""
    }
    END {
      if (buf != "") from(buf, start)
      if (failed) exit 1
      if (mode == "check") {
        for (arg in used) print FILENAME, arg
        exit 0
      }
      if (target == "") {
        class = last
      } else if (tolower(target) in stage) {
        class = stage[tolower(target)]
      } else {
        print "::error::Stage \"" target "\" not found in " FILENAME > "/dev/stderr"
        exit 1
      }
      if (class ~ /^node /) print substr(class, 6)
    }
  ' "$2"
}

if [ "${1:-}" = "--check" ]; then
  shift
  [ "$#" -gt 0 ] || { echo "usage: node_base_arg.sh --check <dockerfile>..." >&2; exit 2; }
  rc=0
  for f in "$@"; do parse check "$f" || rc=1; done   # report every bad file, then fail
  exit "$rc"
fi

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || { echo "usage: node_base_arg.sh <dockerfile> [target]" >&2; exit 2; }
parse resolve "$1" "${2:-}"
