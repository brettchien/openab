# Releasing

## Version Scheme

Versions follow SemVer (e.g. `0.7.0`). Version bumps are controlled via `workflow_dispatch`:

| Method | 效果 | 範例 |
|---|---|---|
| Auto patch (default) | patch bump + beta | `0.6.0 → 0.6.1-beta.1` |
| Auto minor | minor bump + beta | `0.6.0 → 0.7.0-beta.1` |
| Auto major | major bump + beta | `0.6.0 → 1.0.0-beta.1` |
| Manual | 自行指定 | `0.8.0-beta.1` or `0.8.0` |

目前版本已經是 beta 時，auto bump 會忽略 bump type，只把 beta 號碼加 1（e.g. `0.7.0-beta.1 → 0.7.0-beta.2`）。

## Release Flow (Tag-Driven)

> **核心原則：測過什麼就發什麼 (what you tested is what you ship)**
> stable release 不重新 build，直接 promote pre-release 驗證過的 image。

##### Step 1 — 建立 Release PR

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ Maintainer 到 Actions → Release PR → Run workflow                │
  │                                                                  │
  │   選項 A: 留空 version，選 bump type → 自動算 (e.g. 0.7.0-beta.1) │
  │   選項 B: 手動填 version (e.g. 0.8.0-beta.1 or 0.8.0)            │
  │                                                                  │
  │ → release-pr.yml 觸發                                            │
  │ → 更新 Cargo.toml + Chart.yaml version/appVersion               │
  │ → 建立 Release PR (branch: release/v0.7.0-beta.1)                 │
  └─────────────────────────────────────────────────────────────────┘
```

##### Step 2 — Merge Release PR → 自動打 Tag → Build

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ Maintainer review & merge Release PR                             │
  │                                                                  │
  │ → tag-on-merge.yml 偵測 release/ branch merge                   │
  │ → 自動打 tag (e.g. v0.7.0-beta.1)                                 │
  │ → build-operator.yml 觸發 (is_prerelease=true)                  │
  │ → build-core:   shared builder stage × 2 platforms              │
  │ → build-agents: 每個 agent × 2 platforms (amd64 + arm64)        │
  │ → merge-manifests: image tags = 0.7.0-beta.1-<agent>,           │
  │                    beta-<agent>, <short-sha>-<agent>            │
  │ → release-chart:  helm chart → OCI registry                      │
  │ → build-binaries.yml: openab binaries → GitHub Release          │
  └─────────────────────────────────────────────────────────────────┘
        │
        ▼
  ┌─────────────────────────────────────────────────────────────────┐
  │ 部署 pre-release 進行測試：                                       │
  │                                                                  │
  │   helm install openab \                                          │
  │     oci://ghcr.io/openabdev/charts/openab \                      │
  │     --version 0.7.0-beta.1                                         │
  │                                                                  │
  │ 發現 bug？→ 修復 PR merge → 再跑一次 Release PR workflow          │
  │   → 手動指定 v0.7.0-beta.2 → merge → 重新測試                      │
  └─────────────────────────────────────────────────────────────────┘
```

##### Step 3 — Stable Release（Promote）

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ 測試通過後，再跑一次 Release PR workflow                           │
  │ → 手動指定 version: 0.7.0 (不帶 -beta.N)                        │
  │ → merge Release PR                                               │
  │ → tag-on-merge.yml 打 tag v0.7.0                                 │
  │                                                                  │
  │ → build-operator.yml 觸發 (is_prerelease=false)                 │
  │ → promote-stable:                                                │
  │   1. 找到最新的 pre-release tag (v0.7.0-beta.2)                    │
  │   2. 驗證 pre-release image 存在                                  │
  │   3. 每個 agent re-tag 0.7.0-beta.2-<agent> →                   │
  │      0.7.0-<agent>, 0.7-<agent>, stable-<agent>                 │
  │   ⚠️ 不 rebuild，跟 pre-release 是同一個 artifact                 │
  │ → release-chart: helm chart → OCI registry                       │
  └─────────────────────────────────────────────────────────────────┘
```

##### Step 4 — Chart Release（自動）

```
  ┌─────────────────────────────────────────────────────────────────┐
  │ release.yml 偵測到 Chart.yaml 變更 push to main                   │
  │ → chart-releaser 更新 GitHub Pages helm repo index               │
  │ → 附加 install instructions 到 chart release notes               │
  └─────────────────────────────────────────────────────────────────┘
```

## 快速指令參考

```bash
# ── Pre-release ───────────────────────────────────────
# 到 Actions → Release PR → Run workflow
# 留空 version，選 patch → 自動算 0.7.0-beta.1
# 或手動填 version: 0.7.0-beta.1
# → merge 產生的 Release PR → 自動打 tag → build

# ── 第二輪 pre-release（beta.1 有 bug 時）─────────────
# 修 bug → PR merge to main
# 再跑 Release PR workflow，手動填 version: 0.7.0-beta.2
# → merge → 自動打 tag → build

# ── Stable release ────────────────────────────────────
# 跑 Release PR workflow，手動填 version: 0.7.0
# → merge → 自動打 tag → promote beta image (不 rebuild)

# ── 手動重跑（build 失敗時）──────────────────────────
# --ref 讓 workflow checkout tag 的 commit。不加的話會 build main 的 HEAD。
gh workflow run build-operator.yml --ref v0.7.0-beta.1 -f tag=v0.7.0-beta.1
gh workflow run build-operator.yml --ref v0.7.0 -f tag=v0.7.0
```

## Base Image Security Release

Node.js base image 或其他 upstream component 出 security release 時，照這個流程出新版。Support policy 見 [SECURITY.md](SECURITY.md)。只修最新的 minor line，不 backport。

##### Step 1 — 發現 security release

`node-security-check.yml` 每天跑一次。下面任一情況成立時，它會開一個 `node-security` label 的 issue。Issue 已經存在時，它更新同一個 issue。

- Node.js security release 比 Dockerfile 裡的 `NODE22_VERSION` 或 `NODE24_VERSION` 新。
- Node.js security release 比已發佈的 `stable-*` 或 `beta-*` image 裡的 Node.js 新。

有人直接回報 security release 時，也從這一步開始。

##### Step 2 — Bump Node.js pin

1. 找出所有宣告 pin 的 Dockerfile：

   ```bash
   git grep -lE 'ARG NODE[0-9]+_VERSION' -- 'Dockerfile*'
   ```

2. 每個 Dockerfile 只宣告它自己用到的 Node.js major 的 ARG，例如 `Dockerfile.kimi` 只有 `NODE24_VERSION`。在上面列出的每個檔案，把它宣告的 ARG 改成修補後的版本。
3. 開 PR。Smoke tests 從 Dockerfile 讀預期的版本（以 `Dockerfile.unified` 為準），所以 bump 只改 `Dockerfile*`，不用改 workflow。Image 含 Node.js 卻沒有對應的 pin，或各檔案的 pin 不一致時，smoke test 會失敗。CI 通過就代表 image 用了新版 Node.js。

##### Step 3 — 出下一個 beta

1. Bump PR merge 後，照 [Release Flow](#release-flow-tag-driven) 的 Step 1 和 Step 2 出下一個 beta。目前已經是 beta 時，留空 version 就會得到下一個 beta 號碼。
2. 等 `build-operator.yml` 跑完，確認每個含 Node.js 的 agent 都拿到新版。在 repo root 執行。`NODE_AGENTS` 從 `Dockerfile.unified` 取出 `FROM base-node` 和 `FROM node:${NODE<n>_VERSION}...` 的 stage，排除 `base-*`：

   ```bash
   NODE_AGENTS=$(awk 'toupper($1) == "FROM" && toupper($3) == "AS" && $4 !~ /^base-/ && ($2 == "base-node" || $2 ~ /^node:\$\{NODE[0-9]+_VERSION\}/) { print $4 }' Dockerfile.unified)
   for agent in $NODE_AGENTS; do
     printf '%s: ' "$agent"
     docker run --rm --pull always --entrypoint node "ghcr.io/openabdev/openab:beta-$agent" --version
   done
   ```

3. 如果驗證看到舊的 Node.js，修好原因後出下一個 beta，不要重新 push 已經存在的 version tag。

##### Step 4 — Promote 到 stable

Beta 驗證通過後，照 [Release Flow](#release-flow-tag-driven) 的 Step 3 promote。Stable 和 beta 是同一個 image，不 rebuild。舊的 minor line 不出修補版。

Promote 完成後，把 Step 3 的迴圈換成 `stable-$agent` 再跑一次。

##### Step 5 — 發 release note

chart-releaser 建立的 `openab-<version>` GitHub Release 已經有 `release.yml` 附加的安裝說明。把 security note 加在最前面，不要覆蓋原本內容。`VERSION` 是 promote 後的 stable chart version，不加 `v`。例如 `VERSION=0.10.0` 對應 release `openab-0.10.0`：

```bash
VERSION=0.10.0
gh release view "openab-$VERSION" -R openabdev/openab --json body --jq .body > existing.md
if grep -qF '<!-- security-note -->' existing.md; then
  echo "openab-$VERSION already has a security note. Edit it by hand." >&2
else
  { cat security-note.md; printf '\n\n'; cat existing.md; } > release-note.md
  gh release edit "openab-$VERSION" -R openabdev/openab --notes-file release-note.md
fi
```

Release 已經有 `<!-- security-note -->` 時，指令會停下來，不會再加一次。這時在 GitHub 上手動改 release note。

在 `node-security` issue 留下 release 連結。Pin 和 image 都更新後，`node-security-check.yml` 下一次執行會自己關閉這個 issue。

`security-note.md` 用下面的 template。Release note 用英文。`<agent list>` 填 Step 3 的 `NODE_AGENTS`。這個 release 同時帶 breaking change 時，在 note 裡另外加一段 migration（例如 #1567 要求 0.10.0 要有）。

````markdown
<!-- security-note -->
## Security

This release updates the Node.js base image to <22.x.y> and <24.x.y> to fix <CVE IDs, or a link to the Node.js security release>.

- The agent images built on Node.js (<agent list>) get the fix. The other agent images do not contain Node.js.
- The floating tags `stable-<agent>`, `<major.minor>-<agent>`, and `beta-<agent>` now point to this build. A deployment that uses one of them gets the fix on its next image pull.
- Existing version tags are not rebuilt. `<previous-version>-<agent>` and earlier versions keep the old Node.js. To get the fix, move to `<version>-<agent>`.
- Releases older than `<major.minor>` get no fix. Only the latest minor line is supported. See [SECURITY.md](https://github.com/openabdev/openab/blob/main/SECURITY.md).

To check the Node.js version in an image, run:

```bash
docker run --rm --pull always --entrypoint node ghcr.io/openabdev/openab:<version>-<agent> --version
```

The output is the patched Node.js version for that image, `v<22.x.y>` or `v<24.x.y>`.
````

## GitHub Releases

| Release | Tag 格式 | 內容 |
|---|---|---|
| chart-releaser | `openab-0.7.0` | Version Info + Installation instructions，加上 `build-binaries.yml` 的 openab binaries 和 `build-oabctl.yml` 的 oabctl |

## Workflow 對應表

| Workflow | 觸發條件 | 用途 |
|---|---|---|
| `ci.yml` | pull_request (src/Cargo/Dockerfile) | cargo check + clippy + test |
| `release-pr.yml` | workflow_dispatch | 建立 Release PR（更新版本檔案） |
| `tag-on-merge.yml` | release/ PR merge to main | 自動打 tag |
| `build-operator.yml` | tag push `v*` | pre-release: 完整 build / stable: promote；兩者都 push helm chart 到 OCI |
| `build-binaries.yml` | tag push `v*` | build openab binaries，上傳到 `openab-<version>` GitHub Release |
| `release.yml` | Chart.yaml 變更 push to main | chart-releaser 更新 GitHub Pages index |
| `build-oabctl.yml` | Chart.yaml 變更 push to main | build oabctl，上傳到 `openab-<version>` GitHub Release |

## Version 同步

release-pr.yml 在 Release PR 中自動更新以下檔案的版本：

| 檔案 | 欄位 |
|---|---|
| `Cargo.toml` | `version` |
| `charts/openab/Chart.yaml` | `version` |
| `charts/openab/Chart.yaml` | `appVersion` |

`Chart.yaml` 的 `version` 和 `appVersion` 是完整版本（e.g. `0.7.0-beta.1`）。`Cargo.toml` 只寫 stable 部分（e.g. `0.7.0`），讓 main 不帶 pre-release suffix。

## Image Variants

`build-operator.yml` 用 `Dockerfile.unified`，為 `AGENTS` env 列出的每個 agent build 一個 multi-arch image (linux/amd64 + linux/arm64)。所有 agent 都在同一個 repository，用 tag 區分 agent：

```
ghcr.io/openabdev/openab:<version>-<agent>
```

Image tags 依 release 類型不同：

| Tag | Stable (`v0.7.0`) | Pre-release (`v0.7.0-beta.1`) |
|---|---|---|
| `<short-sha>-<agent>` | v (from pre-release) | v |
| `0.7.0-<agent>` / `0.7.0-beta.1-<agent>` | v | v |
| `beta-<agent>` | x | v |
| `0.7-<agent>` | v | x |
| `stable-<agent>` | v | x |

沒有 `latest` tag，也沒有 default agent。完整的 tag 規則見 [docs/image-tags.md](docs/image-tags.md)。Agent 清單以 `build-operator.yml` 的 `AGENTS` env 為準。

舊的 per-agent repositories（`ghcr.io/openabdev/openab-claude`、`ghcr.io/openabdev/openab-codex` 等）已經 deprecated，不再更新。公告後的 grace period 結束時，由 org admin 刪除。

## Installation

##### Helm Repository (GitHub Pages)

```bash
helm repo add openab https://openabdev.github.io/openab
helm repo update
helm install openab openab/openab --version 0.7.0
```

##### OCI Registry

```bash
helm install openab oci://ghcr.io/openabdev/charts/openab --version 0.7.0
```

## 手動操作

| 時機 | 做什麼 |
|---|---|
| 準備 release | Actions → Release PR → Run workflow |
| 需要 beta 測試 | 指定 version 如 `0.7.0-beta.1` |
| 測試通過 | 指定 stable version 如 `0.7.0` → promote |
| build 失敗或需重跑 | `gh workflow run build-operator.yml --ref <tag> -f tag=<tag>` |
| Node.js 或其他 base image 有 security release | 照 [Base Image Security Release](#base-image-security-release) |

## GitHub App 權限

release-pr.yml 和 tag-on-merge.yml 使用 GitHub App token 來建立 PR 和推送 tag。App 需要以下 Repository permissions：

| Permission | Access |
|---|---|
| Contents | Read and write |
| Metadata | Read-only (mandatory) |
| Pull requests | Read and write |

對應的 secrets：`APP_ID`（Client ID）、`APP_PRIVATE_KEY`。

## 限制與注意事項

- **Stable release 必須先有 pre-release**：promote-stable 會查找 `v{version}-*` 的 pre-release tag，找不到就失敗
- **promote 用 version tag 找 image**：不依賴 commit SHA，pre-release 和 stable 可以在不同 commit 上
- **外部用戶不會裝到 pre-release**：`helm install` 預設只拿 stable 版本，pre-release 需明確指定 `--version`
