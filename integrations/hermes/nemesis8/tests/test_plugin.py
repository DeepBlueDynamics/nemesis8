"""Unit tests for the Nemesis8 Hermes plugin.

Tests tool registration, skill registration, argument validation, HTTP protocol formatting,
error handling, and graceful offline degradation using a fake PluginContext
and mocked urllib responses.
"""

import io
import json
import os
import unittest
import urllib.error
from pathlib import Path
from unittest.mock import MagicMock, patch

import integrations.hermes.nemesis8 as plugin
from integrations.hermes.nemesis8 import schemas, tools


class FakePluginContext:
    """Mock implementation of the Hermes PluginContext for unit testing."""

    def __init__(self):
        self.registered_tools = {}
        self.registered_skills = {}

    def register_tool(
        self,
        name,
        toolset,
        schema,
        handler,
        check_fn=None,
        requires_env=None,
        is_async=False,
        description="",
        emoji="",
        override=False,
    ):
        self.registered_tools[name] = {
            "name": name,
            "toolset": toolset,
            "schema": schema,
            "handler": handler,
            "description": description,
            "emoji": emoji,
            "override": override,
        }

    def register_skill(self, name, path, description="", frontmatter=None):
        self.registered_skills[name] = {
            "name": name,
            "path": path,
            "description": description,
            "frontmatter": frontmatter,
        }


class TestPluginRegistration(unittest.TestCase):
    """Verify plugin.register(ctx) conforms to the Hermes plugin SDK contract."""

    def setUp(self):
        self.ctx = FakePluginContext()
        plugin.register(self.ctx)

    def test_registered_tool_names(self):
        expected = {
            "nemesis8_gateway_status",
            "nemesis8_agent_list",
            "nemesis8_agent_spawn",
            "nemesis8_agent_stop",
            "nemesis8_agent_kill",
        }
        self.assertEqual(set(self.ctx.registered_tools.keys()), expected)

    def test_toolset_assignment(self):
        for name, tool_def in self.ctx.registered_tools.items():
            self.assertEqual(tool_def["toolset"], "nemesis8")

    def test_tool_schemas(self):
        for name, tool_def in self.ctx.registered_tools.items():
            schema = tool_def["schema"]
            self.assertIsInstance(schema, dict)
            self.assertEqual(schema["name"], name)
            self.assertIn("description", schema)
            self.assertIn("parameters", schema)
            self.assertEqual(schema["parameters"].get("type"), "object")
            self.assertIn("properties", schema["parameters"])
            self.assertIn("required", schema["parameters"])

    def test_handlers_callable(self):
        for name, tool_def in self.ctx.registered_tools.items():
            self.assertTrue(callable(tool_def["handler"]))

    def test_registered_skill(self):
        self.assertIn("nemesis8-fleet", self.ctx.registered_skills)
        skill_entry = self.ctx.registered_skills["nemesis8-fleet"]
        self.assertEqual(skill_entry["name"], "nemesis8-fleet")
        self.assertEqual(skill_entry["description"], "Operate the Nemesis8 gateway and agent fleet")
        skill_path = skill_entry["path"]
        self.assertIsInstance(skill_path, Path)
        self.assertEqual(skill_path.name, "SKILL.md")
        self.assertTrue(skill_path.is_file())


class TestArgumentValidation(unittest.TestCase):
    """Verify tool parameter validation before any network dispatch."""

    def test_agent_spawn_missing_prompt(self):
        for invalid_args in [{}, None, {"prompt": ""}, {"prompt": "   "}, {"prompt": 123}]:
            out = tools.agent_spawn(invalid_args)
            data = json.loads(out)
            self.assertEqual(data["status"], "error")
            self.assertIn("Missing or", data["error"])

    def test_agent_spawn_invalid_optional_types(self):
        cases = [
            ({"prompt": "ok", "provider": 123}, "provider"),
            ({"prompt": "ok", "provider": ""}, "provider"),
            ({"prompt": "ok", "model": []}, "model"),
            ({"prompt": "ok", "model": "  "}, "model"),
            ({"prompt": "ok", "workspace": {}}, "workspace"),
            ({"prompt": "ok", "workspace": "\t"}, "workspace"),
        ]
        for args, param in cases:
            out = tools.agent_spawn(args)
            data = json.loads(out)
            self.assertEqual(data["status"], "error")
            self.assertIn(f"Invalid type for parameter '{param}'", data["error"])

    def test_agent_stop_missing_id(self):
        for invalid_args in [{}, None, {"id": ""}, {"id": "   "}, {"id": 123}]:
            out = tools.agent_stop(invalid_args)
            data = json.loads(out)
            self.assertEqual(data["status"], "error")
            self.assertIn("Missing or", data["error"])

    def test_agent_kill_missing_id(self):
        out = tools.agent_kill({})
        data = json.loads(out)
        self.assertEqual(data["status"], "error")
        self.assertIn("Missing or", data["error"])


class TestGatewayProtocol(unittest.TestCase):
    """Verify HTTP request construction, headers, payloads, and URL encoding."""

    def setUp(self):
        self.env_patcher = patch.dict(
            os.environ,
            {
                "GATEWAY_URL": "http://127.0.0.1:9801",
                "NEMESIS8_AUTH_TOKEN": "test-secret-token",
            },
            clear=False,
        )
        self.env_patcher.start()

    def tearDown(self):
        self.env_patcher.stop()

    @patch("urllib.request.urlopen")
    def test_gateway_status_success(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b'{"uptime_secs": 4200, "agent_count": 3}'
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.gateway_status()
        data = json.loads(out)

        self.assertEqual(data["uptime_secs"], 4200)
        self.assertEqual(data["agent_count"], 3)

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_full_url(), "http://127.0.0.1:9801/status")
        self.assertEqual(req.get_method(), "GET")
        self.assertEqual(req.headers.get("Authorization"), "Bearer test-secret-token")

    @patch("urllib.request.urlopen")
    def test_agent_list_success(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b'[{"id": "agent-1", "status": "running"}]'
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.agent_list()
        data = json.loads(out)

        self.assertEqual(data["status"], "ok")
        self.assertEqual(data["data"], [{"id": "agent-1", "status": "running"}])

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_full_url(), "http://127.0.0.1:9801/agents")
        self.assertEqual(req.get_method(), "GET")

    @patch("urllib.request.urlopen")
    def test_agent_spawn_success(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b'{"id": "host/n8-agent-123", "status": "spawned"}'
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.agent_spawn({
            "prompt": "Write a test suite",
            "provider": "claude",
            "model": "claude-3-7-sonnet-20250219",
            "workspace": "/workspace/nemesis8",
        })
        data = json.loads(out)

        self.assertEqual(data["id"], "host/n8-agent-123")
        self.assertEqual(data["status"], "spawned")

        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_full_url(), "http://127.0.0.1:9801/agents/spawn")
        self.assertEqual(req.get_method(), "POST")
        payload = json.loads(req.data.decode("utf-8"))
        self.assertEqual(
            payload,
            {
                "prompt": "Write a test suite",
                "provider": "claude",
                "model": "claude-3-7-sonnet-20250219",
                "workspace": "/workspace/nemesis8",
            },
        )

    @patch("urllib.request.urlopen")
    def test_agent_stop_encodes_id(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b'{"ok": true}'
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.agent_stop({"id": "host/my agent#1"})
        data = json.loads(out)

        self.assertTrue(data.get("ok"))
        req = mock_urlopen.call_args[0][0]
        self.assertEqual(req.get_full_url(), "http://127.0.0.1:9801/agents/host%2Fmy%20agent%231/kill")
        self.assertEqual(req.get_method(), "POST")


class TestOfflineAndErrorHandling(unittest.TestCase):
    """Verify handling when gateway is offline, URL is invalid, or returns non-JSON/HTTP errors."""

    @patch("urllib.request.urlopen")
    def test_gateway_offline_urlerror(self, mock_urlopen):
        mock_urlopen.side_effect = urllib.error.URLError("Connection refused")

        out = tools.gateway_status()
        data = json.loads(out)

        self.assertEqual(data["status"], "gateway_not_running")
        self.assertIn("The nemesis8 gateway is not running", data["message"])
        self.assertIn("n8 serve --background", data["message"])

    @patch("urllib.request.urlopen")
    def test_gateway_offline_timeout(self, mock_urlopen):
        import socket
        mock_urlopen.side_effect = socket.timeout("Timed out")

        out = tools.agent_list()
        data = json.loads(out)

        self.assertEqual(data["status"], "gateway_not_running")
        self.assertIn("The nemesis8 gateway is not running", data["message"])

    @patch("urllib.request.urlopen")
    def test_gateway_http_error_response(self, mock_urlopen):
        fp = io.BytesIO(b'{"error": "agent not found"}')
        mock_urlopen.side_effect = urllib.error.HTTPError(
            url="http://127.0.0.1:9801/agents/missing/kill",
            code=404,
            msg="Not Found",
            hdrs={},
            fp=fp,
        )

        out = tools.agent_stop({"id": "missing"})
        data = json.loads(out)

        self.assertEqual(data["status"], "error")
        self.assertEqual(data["http_code"], 404)
        self.assertIn("404", data["error"])

    @patch("urllib.request.urlopen")
    def test_non_json_success_response(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b"<html>Gateway upstream error</html>"
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.gateway_status()
        data = json.loads(out)

        self.assertEqual(data["status"], "error")
        self.assertIn("Invalid non-JSON response from gateway", data["error"])

    @patch("urllib.request.urlopen")
    def test_empty_success_response(self, mock_urlopen):
        mock_resp = MagicMock()
        mock_resp.read.return_value = b""
        mock_urlopen.return_value.__enter__.return_value = mock_resp

        out = tools.gateway_status()
        data = json.loads(out)

        self.assertEqual(data, {"ok": True})

    def test_invalid_gateway_url_structured_error(self):
        with patch.dict(os.environ, {"GATEWAY_URL": "invalid-url-without-scheme"}):
            out = tools.gateway_status()
            data = json.loads(out)

            self.assertEqual(data["status"], "error")
            self.assertIn("Invalid GATEWAY_URL", data["error"])


if __name__ == "__main__":
    unittest.main()
