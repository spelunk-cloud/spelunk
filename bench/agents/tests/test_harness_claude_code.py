"""Tests for bench/agents/harness_claude_code.py.

Run:
    uv run --with pytest pytest bench/agents/tests/ -v

Network-free, no DEEPSEEK_API_KEY, no claude binary required.
"""

import json
import os
import stat
import subprocess
import sys
from pathlib import Path

import pytest
from harness_claude_code import (
    SERVER_NAME,
    _deepseek_anthropic_env,
    build_claude_cmd,
    write_mcp_config,
)
from spelunk_mcp_server import mcp_tool_names_for_condition

CONDITIONS = ["baseline", "spelunk_search", "spelunk_full"]
SPELUNK_CONDITIONS = ["spelunk_search", "spelunk_full"]

HARNESS_CLAUDE_CODE = Path(__file__).resolve().parents[1] / "harness_claude_code.py"


class TestDeepseekAnthropicEnv:
    """_deepseek_anthropic_env is the function responsible for redirecting
    Claude Code's Anthropic client at DeepSeek. Its own docstring calls out
    the stakes: "silently picking the wrong [env var] and falling through to
    the user's own Anthropic credentials... would misattribute a
    Claude-native run as a DeepSeek one." These are direct assertions on its
    output, rather than only exercising it indirectly through a fake `claude`
    shim that ignores env entirely (as the --no-deepseek provenance-contract
    test does)."""

    def test_sets_auth_token_not_just_api_key(self, tmp_path):
        # The var name DeepSeek's docs actually specify (see module
        # docstring/README citation) -- this is the one that must be right.
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=tmp_path / "cfg",
        )
        assert env["ANTHROPIC_AUTH_TOKEN"] == "sk-test-key"

    def test_sets_belt_and_braces_api_key_alias_too(self, tmp_path):
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=tmp_path / "cfg",
        )
        assert env["ANTHROPIC_API_KEY"] == "sk-test-key"

    def test_sets_base_url_and_model(self, tmp_path):
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="http://127.0.0.1:4000",
            claude_config_dir=tmp_path / "cfg",
        )
        assert env["ANTHROPIC_BASE_URL"] == "http://127.0.0.1:4000"
        assert env["ANTHROPIC_MODEL"] == "deepseek-v4-flash"

    def test_does_not_mutate_real_process_environment(self, monkeypatch, tmp_path):
        monkeypatch.delenv("ANTHROPIC_BASE_URL", raising=False)
        _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=tmp_path / "cfg",
        )
        assert "ANTHROPIC_BASE_URL" not in os.environ

    def test_preserves_unrelated_ambient_env_vars(self, monkeypatch, tmp_path):
        monkeypatch.setenv("SOME_UNRELATED_VAR", "keep-me")
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=tmp_path / "cfg",
        )
        assert env["SOME_UNRELATED_VAR"] == "keep-me"

    def test_sets_claude_config_dir_to_the_isolated_path(self, tmp_path):
        isolated = tmp_path / "isolated-claude-config"
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=isolated,
        )
        assert env["CLAUDE_CONFIG_DIR"] == str(isolated)

    def test_overrides_any_ambient_claude_config_dir(self, monkeypatch, tmp_path):
        # A host machine with its own stored login sets CLAUDE_CONFIG_DIR
        # (or defaults to ~/.claude) -- that ambient value is exactly what
        # let `claude` fall through to the wrong credential and 401 against
        # DeepSeek (reproduced directly on a host with a Claude Code login).
        # The isolated path must win, not merge with or defer to it.
        monkeypatch.setenv("CLAUDE_CONFIG_DIR", "/some/host/login/dir")
        isolated = tmp_path / "isolated-claude-config"
        env = _deepseek_anthropic_env(
            api_key="sk-test-key",
            model="deepseek-v4-flash",
            base_url="https://api.deepseek.com/anthropic",
            claude_config_dir=isolated,
        )
        assert env["CLAUDE_CONFIG_DIR"] == str(isolated)
        assert env["CLAUDE_CONFIG_DIR"] != "/some/host/login/dir"


def _cmd(condition, mcp_config_path=None):
    return build_claude_cmd(
        prompt="fix it",
        effort="high",
        thinking=False,
        condition=condition,
        mcp_config_path=mcp_config_path,
    )


class TestStrictMcpConfig:
    """The bench host has its own MCP servers configured. Without
    --strict-mcp-config they load into *both* arms, so baseline and spelunk
    are contaminated alike and the numbers are unpublishable. Verified on
    this host during adapter work: a host server appeared in the run's
    mcp_servers without the flag, and mcp_servers was empty with it.
    """

    @pytest.mark.parametrize("condition", CONDITIONS)
    def test_passed_in_every_arm_including_baseline(self, condition, tmp_path):
        assert "--strict-mcp-config" in _cmd(condition, tmp_path / "mcp.json")

    def test_passed_on_baseline_even_with_no_mcp_config(self):
        # How main() actually calls it on baseline: flag still required.
        assert "--strict-mcp-config" in _cmd("baseline")


class TestMcpConfigIsConditionGated:
    def test_only_spelunk_arms_load_the_bench_server(self, tmp_path):
        # One predicate, both directions: the absence assertion below is only
        # evidence because the same check finds the flag when it is present.
        cfg = tmp_path / "mcp.json"
        assert "--mcp-config" in _cmd("spelunk_search", cfg)
        assert "--mcp-config" not in _cmd("baseline", cfg)

    def test_baseline_ignores_a_config_path_it_was_handed(self, tmp_path):
        # Gating is on the condition, not on the caller remembering to pass
        # None: a baseline arm that quietly gained spelunk tools would
        # invalidate results in the opposite direction.
        cfg = tmp_path / "mcp.json"
        assert str(cfg) not in _cmd("baseline", cfg)

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_spelunk_arms_point_at_the_written_config(self, condition, tmp_path):
        cfg = tmp_path / "mcp.json"
        cmd = _cmd(condition, cfg)
        assert cmd[cmd.index("--mcp-config") + 1] == str(cfg)


class TestAllowedTools:
    """--permission-mode acceptEdits covers file edits, not MCP tool calls. A
    headless -p run that hits a permission prompt is a lost cell."""

    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_names_every_exposed_tool_in_full(self, condition, tmp_path):
        cmd = _cmd(condition, tmp_path / "mcp.json")
        expected = mcp_tool_names_for_condition(condition)
        assert set(expected) <= set(cmd)
        # Named individually rather than relying on the `mcp__spelunk`
        # server-wide shorthand, which is unverified.
        assert f"mcp__{SERVER_NAME}" not in cmd

    def test_allow_list_is_condition_gated(self, tmp_path):
        cfg = tmp_path / "mcp.json"
        search = _cmd("spelunk_search", cfg)
        full = _cmd("spelunk_full", cfg)
        assert f"mcp__{SERVER_NAME}__spelunk_graph" not in search
        assert f"mcp__{SERVER_NAME}__spelunk_graph" in full

    def test_absent_on_baseline(self, tmp_path):
        cfg = tmp_path / "mcp.json"
        assert "--allowedTools" in _cmd("spelunk_search", cfg)
        assert "--allowedTools" not in _cmd("baseline", cfg)


class TestWriteMcpConfig:
    @pytest.mark.parametrize("condition", SPELUNK_CONDITIONS)
    def test_registers_the_bench_server_under_mcp_servers(self, condition, tmp_path):
        repo = tmp_path / "repo"
        repo.mkdir()
        path = write_mcp_config(tmp_path, condition, repo, None)
        server = json.loads(path.read_text())["mcpServers"][SERVER_NAME]
        assert server["args"][0].endswith("spelunk_mcp_server.py")
        assert server["args"][server["args"].index("--condition") + 1] == condition
        assert server["env"]["SPELUNK_SECRET_STORE"] == "file"

    def test_written_outside_the_task_repo(self, tmp_path):
        # Anything written inside repo_path can land in the extracted patch.
        repo = tmp_path / "repo"
        repo.mkdir()
        scratch = tmp_path / "scratch"
        scratch.mkdir()
        path = write_mcp_config(scratch, "spelunk_search", repo, None)
        assert repo not in path.parents


FAKE_CLAUDE_CAPTURE_CONFIG_DIR_SHIM = """#!/usr/bin/env bash
# Fake `claude` binary for offline testing of main()'s subprocess env: records
# CLAUDE_CONFIG_DIR to a file outside the config dir itself (the harness
# rmtree's the config dir on exit, so the test can't inspect it in place
# afterward) before emitting a minimal --output-format json result.
echo "$CLAUDE_CONFIG_DIR" > "$CLAUDE_CONFIG_DIR_CAPTURE_FILE"
echo '{"num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}, "is_error": false, "modelUsage": {}}'
"""


@pytest.fixture()
def fake_claude_capturing_config_dir(tmp_path):
    bin_dir = tmp_path / "fakebin"
    bin_dir.mkdir()
    claude_path = bin_dir / "claude"
    claude_path.write_text(FAKE_CLAUDE_CAPTURE_CONFIG_DIR_SHIM)
    claude_path.chmod(
        claude_path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH
    )

    capture_file = tmp_path / "captured-config-dir.txt"
    # Simulate a host machine that already has its own config dir (with a
    # stored login) set via the ambient environment -- the isolation fix
    # must override this on the DeepSeek path, not inherit it.
    ambient_config_dir = tmp_path / "ambient-host-claude-config"
    ambient_config_dir.mkdir()

    env = dict(os.environ)
    env["PATH"] = f"{bin_dir}:{env.get('PATH', '')}"
    env["CLAUDE_CONFIG_DIR_CAPTURE_FILE"] = str(capture_file)
    env["CLAUDE_CONFIG_DIR"] = str(ambient_config_dir)
    return env, capture_file, ambient_config_dir


@pytest.fixture()
def throwaway_repo(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "Test"], cwd=repo, check=True)
    (repo / "README.md").write_text("hello\n")
    subprocess.run(["git", "add", "README.md"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "init"], cwd=repo, check=True)
    return repo


class TestClaudeConfigDirIsolation:
    """End-to-end regression coverage for the auth bug: on this exact host,
    `claude -p` sends an ambient stored login credential instead of the
    injected ANTHROPIC_AUTH_TOKEN/ANTHROPIC_API_KEY, and DeepSeek 401s.
    Driven through main() as a real subprocess (like
    test_provenance_contract.py) with a fake `claude` that records which
    CLAUDE_CONFIG_DIR it actually received, since that's the mechanism the
    fix relies on and unit-testing _deepseek_anthropic_env alone wouldn't
    prove main() actually wires it through and cleans it up."""

    def test_deepseek_path_overrides_the_ambient_config_dir(
        self, fake_claude_capturing_config_dir, throwaway_repo, tmp_path
    ):
        env, capture_file, ambient_config_dir = fake_claude_capturing_config_dir
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__cfgdir-1",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--model",
                "deepseek-v4-flash",
                "--api-key",
                "sk-fake-not-a-real-key",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

        assert result.returncode == 0, result.stderr
        used_config_dir = capture_file.read_text().strip()
        assert used_config_dir, "claude subprocess never saw CLAUDE_CONFIG_DIR"
        assert used_config_dir != str(ambient_config_dir)

    def test_isolated_config_dir_is_removed_after_the_run(
        self, fake_claude_capturing_config_dir, throwaway_repo, tmp_path
    ):
        env, capture_file, _ambient_config_dir = fake_claude_capturing_config_dir
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__cfgdir-2",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--model",
                "deepseek-v4-flash",
                "--api-key",
                "sk-fake-not-a-real-key",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

        assert result.returncode == 0, result.stderr
        used_config_dir = Path(capture_file.read_text().strip())
        # Cleaned up in the same finally block as the MCP scratch dir --
        # a bench host running many cells shouldn't accumulate one throwaway
        # config dir per task.
        assert not used_config_dir.exists()

    def test_no_deepseek_path_keeps_the_ambient_config_dir(
        self, fake_claude_capturing_config_dir, throwaway_repo, tmp_path
    ):
        # --no-deepseek is documented as "uses Claude Code's own default
        # Anthropic credentials/model unchanged" -- isolation must NOT kick
        # in here, or a future native-Claude-model cell would lose its real
        # login along with the ambient credential it's supposed to use.
        env, capture_file, ambient_config_dir = fake_claude_capturing_config_dir
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__cfgdir-3",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--no-deepseek",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

        assert result.returncode == 0, result.stderr
        used_config_dir = capture_file.read_text().strip()
        assert used_config_dir == str(ambient_config_dir)

    def test_shim_endpoint_kind_gets_the_same_isolation_as_anthropic_compat(
        self, fake_claude_capturing_config_dir, throwaway_repo, tmp_path
    ):
        # _deepseek_anthropic_env is called from the same branch (guarded
        # only on args.no_deepseek) for both endpoint kinds, but nothing
        # above proves the shim path specifically -- an --endpoint-kind
        # check added later that special-cased "anthropic-compat" would
        # silently leave the shim arm on ambient auth again.
        env, capture_file, ambient_config_dir = fake_claude_capturing_config_dir
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__cfgdir-4",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--model",
                "deepseek-v4-flash",
                "--api-key",
                "sk-fake-not-a-real-key",
                "--endpoint-kind",
                "shim",
                "--shim-base-url",
                "http://127.0.0.1:4000",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

        assert result.returncode == 0, result.stderr
        used_config_dir = capture_file.read_text().strip()
        assert used_config_dir, "claude subprocess never saw CLAUDE_CONFIG_DIR"
        assert used_config_dir != str(ambient_config_dir)


class TestClaudeConfigDirCleanupOnFailure:
    """The isolation fix creates claude_config_dir before the try/finally
    that cleans it up, same lifecycle as the pre-existing MCP scratch_dir.
    The happy-path tests above only prove cleanup when `claude` exits 0 --
    this proves it also happens when run_claude_code's subprocess.run call
    itself raises (e.g. `claude` genuinely missing from PATH), which
    propagates past the point cleanup was expected to run rather than being
    caught anywhere."""

    def test_temp_dirs_are_not_leaked_when_the_claude_binary_is_missing(
        self, throwaway_repo, tmp_path
    ):
        tmp_root = tmp_path / "tmproot"
        tmp_root.mkdir()
        empty_bin_dir = tmp_path / "empty-bin"
        empty_bin_dir.mkdir()
        issue_file = tmp_path / "ISSUE.txt"
        issue_file.write_text("Fix the bug.")

        env = dict(os.environ)
        env["PATH"] = str(empty_bin_dir)  # no `claude` resolvable anywhere
        env["TMPDIR"] = str(tmp_root)

        result = subprocess.run(
            [
                sys.executable,
                str(HARNESS_CLAUDE_CODE),
                "--task-id",
                "fake__cleanup-1",
                "--repo-path",
                str(throwaway_repo),
                "--issue",
                str(issue_file),
                "--model",
                "deepseek-v4-flash",
                "--api-key",
                "sk-fake-not-a-real-key",
            ],
            capture_output=True,
            text=True,
            timeout=30,
            env=env,
        )

        # FileNotFoundError from subprocess.run propagates uncaught -- this
        # must fail loudly, not silently succeed with a fabricated result.
        assert result.returncode != 0
        assert "FileNotFoundError" in result.stderr
        leaked = list(tmp_root.iterdir())
        assert leaked == [], f"temp dirs leaked on the exception path: {leaked}"


class TestMaxTurnsNotEnforced:
    """Secondary bug from the same story: max_turns was recorded in
    provenance but never wired into the actual `claude` invocation. The
    installed CLI has no turn-cap flag (checked `claude --help`), so the fix
    is documenting that explicitly rather than fabricating a flag that
    doesn't exist -- mirrors harness_opencode.py's identical
    accepted-but-not-enforced pattern (see README 'Adapter notes')."""

    def test_never_passed_to_the_claude_cli(self):
        cmd = build_claude_cmd(
            prompt="fix it",
            effort="high",
            thinking=False,
            condition="baseline",
            mcp_config_path=None,
        )
        assert "--max-turns" not in cmd

    def test_help_documents_it_is_not_enforced(self):
        result = subprocess.run(
            [sys.executable, str(HARNESS_CLAUDE_CODE), "--help"],
            capture_output=True,
            text=True,
            timeout=15,
        )
        assert result.returncode == 0
        help_text = " ".join(result.stdout.split()).lower()
        assert "not enforced" in help_text
