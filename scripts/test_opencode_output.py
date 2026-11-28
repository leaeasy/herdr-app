import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("opencode_output", Path(__file__).with_name("opencode-output.py"))
output = importlib.util.module_from_spec(spec)
spec.loader.exec_module(output)


def assistant(parts):
    return {"type": "assistant", "time": {"completed": 1}, "content": parts}


class OpenCodeOutputTests(unittest.TestCase):
    def test_final_text_only(self):
        answer = assistant([{"type": "reasoning", "text": "private"},
                            {"type": "text", "text": "回答\n原文"},
                            {"type": "tool", "text": "tool result"}])
        self.assertEqual(output.last_answer([answer, {"type": "idle", "outcome": "succeeded"}]),
                         {"status": "ok", "text": "回答\n原文"})

    def test_never_borrows_previous_answer(self):
        old = [assistant([{"type": "text", "text": "old"}]),
               {"type": "idle", "outcome": "succeeded"}]
        for tail in [[{"type": "user"}],
                     [{"type": "user"}, {"type": "idle", "outcome": "succeeded"}],
                     [{"type": "idle", "outcome": "interrupted"}],
                     [assistant([{"type": "tool"}]), {"type": "idle", "outcome": "succeeded"}]]:
            self.assertIsNone(output.last_answer(old + tail)["text"])

    def test_statistics_cover_whole_turn_not_previous_turn_or_last_prompt(self):
        first = assistant([{"type": "tool"}])
        first["tokens"] = {"output": 100}
        final = assistant([{"type": "text", "text": "done"}])
        final["tokens"] = {"output": 200}
        messages = [
            {"type": "assistant", "tokens": {"output": 999}},
            {"type": "idle", "outcome": "succeeded", "time": {"created": 900}},
            {"type": "user", "time": {"created": 1000}}, first,
            {"type": "synthetic", "time": {"created": 5000}}, final,
            {"type": "idle", "outcome": "succeeded", "time": {"created": 84400}},
        ]
        self.assertEqual(output.last_answer(messages), {
            "status": "ok", "text": "done", "duration_ms": 83400, "output_tokens": 300})

    def test_missing_or_invalid_statistics_are_omitted(self):
        final = assistant([{"type": "text", "text": "done"}])
        messages = [{"type": "user"}, final, {"type": "idle", "outcome": "succeeded"}]
        self.assertEqual(output.last_answer(messages), {"status": "ok", "text": "done"})
        for value in [None, -1, True, float("nan"), float("inf")]:
            final["tokens"] = {"output": value}
            self.assertNotIn("output_tokens", output.last_answer(messages))
        final["tokens"] = {"output": 0}
        self.assertEqual(output.last_answer(messages)["output_tokens"], 0)
        messages.insert(1, assistant([]))
        self.assertNotIn("output_tokens", output.last_answer(messages))

    def test_model_is_from_final_reply_not_earlier_calls(self):
        first = assistant([{"type": "tool"}])
        first["model"] = {"id": "earlier-model"}
        final = assistant([{"type": "text", "text": "done"}])
        final["model"] = {"id": "gpt-6.1-sol", "providerID": "provider"}
        messages = [first, final, {"type": "idle", "outcome": "succeeded"}]
        self.assertEqual(output.last_answer(messages)["model"], "gpt-6.1-sol")
        for model in [{}, {"id": " "}, {"id": 123}]:
            final["model"] = model
            self.assertNotIn("model", output.last_answer(messages))

    def test_generation_rate_uses_final_reply_stream_time(self):
        final = assistant([{"type": "text", "text": "done"}])
        final["time"] = {"created": 1000, "firstToken": 2000, "streamed": 12000, "completed": 20000}
        final["tokens"] = {"output": 507}
        messages = [final, {"type": "idle", "outcome": "succeeded"}]
        self.assertEqual(output.last_answer(messages)["tokens_per_second"], 50.7)
        final["time"]["streamed"] = 2000
        self.assertNotIn("tokens_per_second", output.last_answer(messages))
        del final["time"]["firstToken"]
        self.assertNotIn("tokens_per_second", output.last_answer(messages))

    def test_unfinished_and_failed_are_not_answers(self):
        for outcome in ["failed", "interrupted"]:
            self.assertIsNone(output.last_answer([
                assistant([{"type": "text", "text": "partial"}]),
                {"type": "idle", "outcome": outcome}])["text"])
        self.assertIsNone(output.last_answer([])["text"])

    def test_completed_session_survives_cleared_records(self):
        responses = [{"address": "t1", "records": {}}, {"data": [
            assistant([{"type": "text", "text": "final answer"}]),
            {"type": "idle", "outcome": "succeeded"}]}]
        with patch("sys.argv", ["adapter", "t1", "--completed-session", "ses_exact"]), \
             patch.object(output, "read_json", side_effect=responses) as read, \
             patch("builtins.print") as printed:
            output.main()
        self.assertEqual(read.call_args.args[0][-1], "/api/session/ses_exact/context")
        self.assertIn('"text": "final answer"', printed.call_args.args[0])

    def test_completed_session_rejects_conflicting_terminal(self):
        with patch("sys.argv", ["adapter", "t1", "--completed-session", "ses_old"]), \
             patch.object(output, "read_json", return_value={
                 "address": "t1", "records": {"ses_new": {"app": "opencode"}}}):
            with self.assertRaises(ValueError):
                output.main()

    def test_remote_terminal_is_not_queried(self):
        with patch("sys.argv", ["adapter", "remote/t1", "--completed-session", "ses_exact"]), \
             patch.object(output, "read_json", return_value={
                 "address": "remote/t1", "records": {}}) as read:
            with self.assertRaises(ValueError):
                output.main()
        self.assertEqual(read.call_count, 1)


if __name__ == "__main__":
    unittest.main()
