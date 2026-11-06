> 历史专题记录：保留设计背景、协议细节和当时的验收结果；其中“当前/尚未”描述可能早于最新实现。现行用法以[入门](../getting-started.md)和[参考](../reference.md)为准。

# Pinned DSH configuration audit

Audit date: 2026-11-04. Scope: official `deepseek-harness-runtime-bin==0.2.1rc2`, Linux x86_64, Agenct `sdk-minimal` profile and `adapters/dsh/no-tools.patch.yml`.

## Result

The actual packaged runtime successfully composed both the default and patched profile with `--dump-config`. Both contain 35 unique rows. The patch changes exactly these six rows to literal `disabled: true`:

- `tool-fs` (`@deepseek-ai/dsh-tool-fs`)
- `tool-working-directory` (`@deepseek-ai/dsh-tool-working-directory`)
- `persistent-bash` (`@deepseek-ai/dsh-tool-bash-persistent`)
- `persistent-pwsh` (`@deepseek-ai/dsh-tool-pwsh-persistent`)
- `session-log-deepseek` (`@deepseek-ai/dsh-session-log-deepseek`)
- `plugin-package-inventory-deepseek` (`@deepseek-ai/dsh-plugin-package-inventory-deepseek`)

No remaining enabled row has a `@deepseek-ai/dsh-tool-` package name. This is evidence that the pinned runtime accepts and composes Agenct's patch, **not an attestation of the live tool registry or a sandbox guarantee**. Package names are not a security boundary. The generic tools registry, local filesystem, attachment, terminal, subprocess and sandbox services remain configured; the sandbox policy still says `danger-full-access`. These services were not mounted during this audit.

No provider credentials, model requests, session prompts or tool invocations were used. Runtime environment was cleared and HOME/DSH_HOME pointed to a fresh temporary directory. The config-dump path may create its profile files there, but does not boot plugins or evaluate `!!js` expressions. Both dumps exited 0 with empty stderr.

## Artifact identity

PyPI artifact: `deepseek_harness_runtime_bin-0.2.1rc2-py3-none-manylinux_2_28_x86_64.whl`.

Verified wheel SHA-256 against version-specific PyPI metadata:

`0a0a4eb85a22cd9339972e6a5ced77a27ec791353a5b220e3e10b8cdbbdeca39`

The wheel's embedded `deepseek-harness-runtime.json` says `0.0.0-dev`. Do not use that embedded field as release provenance; this audit identifies the exact published wheel and hash above. The runtime binary is not committed to this repository.

## Reproduction

After obtaining and verifying the pinned official wheel, extract its runtime executable and set `RUNTIME` to its absolute path. Set `REPO` to this checkout. Use a new empty temporary directory and do not reuse a personal DSH home.

```sh
AUDIT_HOME=$(mktemp -d)
chmod 700 "$AUDIT_HOME"
env -i PATH=/usr/bin:/bin HOME="$AUDIT_HOME" DSH_HOME="$AUDIT_HOME" \
  "$RUNTIME" --profile sdk-minimal --dump-config > default.yml
env -i PATH=/usr/bin:/bin HOME="$AUDIT_HOME" DSH_HOME="$AUDIT_HOME" \
  "$RUNTIME" --profile sdk-minimal \
  --patch "$REPO/adapters/dsh/no-tools.patch.yml" --dump-config > patched.yml
```

Compare YAML structurally, preserving `!!js` as unevaluated data. Assert unique IDs, equal row count, exactly the six changed rows above, and literal `disabled: true` on each. Do not count a grep or static fixture test as an actual runtime test. No ordinary unit test was added because it would not establish anything beyond these measured dumps.

## Remaining verification boundary

The pinned SDK server dispatch supports initialize, session prompt/resume/wait, working-directory get/set and shutdown. There is no read-only tool-inventory RPC. Python SDK initialization also resolves the provider configuration; launching a normal session is not a credential-free substitute for inventory inspection.

Before claiming a verified no-tools runtime, add an independently reviewed mechanism to inspect the **mounted registry** without invoking tools or a provider, and check that its visibility matches the model's effective tool list. That would require runtime boot and is outside this boot-free audit. Also test all supported OS artifacts and re-audit any version/profile/patch change. Neither this audit nor the no-tools patch enables the separately disabled persistent-session or Docker-executor features.

## Primary sources

- [Version-specific PyPI artifact metadata](https://pypi.org/pypi/deepseek-harness-runtime-bin/0.2.1rc2/json)
- [Pinned sdk-minimal profile](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.2/packages/bundle/sdk-minimal/cordis.patch.yml)
- [Boot-free configuration dump implementation](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.2/apps/cli/src/dump-config.ts)
- [Pinned SDK server dispatch and initialization](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.2.1-rc.2/packages/sdk/server/src/server.ts)
