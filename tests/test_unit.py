"""Unit tests for core Python modules — covers API-level contracts that the
integration test file (test_porting_workspace.py) validates only through CLI
subprocess calls."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from src.commands import (
    PORTED_COMMANDS,
    CommandExecution,
    built_in_command_names,
    command_names,
    execute_command,
    find_commands,
    get_command,
    get_commands,
    render_command_index,
)
from src.context import PortContext, build_port_context, render_context
from src.cost_tracker import CostTracker
from src.costHook import apply_cost_hook
from src.deferred_init import DeferredInitResult, run_deferred_init
from src.history import HistoryEvent, HistoryLog
from src.models import (
    PermissionDenial,
    PortingBacklog,
    PortingModule,
    Subsystem,
    UsageSummary,
)
from src.parity_audit import ParityAuditResult, run_parity_audit
from src.permissions import ToolPermissionContext
from src.query_engine import QueryEngineConfig, QueryEnginePort, TurnResult
from src.session_store import StoredSession, load_session, save_session
from src.task import PortingTask
from src.tasks import default_tasks
from src.tools import (
    PORTED_TOOLS,
    ToolExecution,
    execute_tool,
    filter_tools_by_permission_context,
    find_tools,
    get_tool,
    get_tools,
    render_tool_index,
    tool_names,
)
from src.transcript import TranscriptStore


class TestPortingTask(unittest.TestCase):
    """Tests for the fixed task.py / tasks.py — previously broken by circular import."""

    def test_porting_task_is_frozen(self) -> None:
        task = PortingTask('demo', 'A demo task')
        with self.assertRaises(AttributeError):
            task.name = 'changed'  # type: ignore[misc]

    def test_default_tasks_returns_nonempty_list(self) -> None:
        tasks = default_tasks()
        self.assertGreater(len(tasks), 0)
        for task in tasks:
            self.assertIsInstance(task, PortingTask)

    def test_default_tasks_includes_parity_audit(self) -> None:
        tasks = default_tasks()
        names = [t.name for t in tasks]
        self.assertIn('parity-audit', names)


class TestUsageSummary(unittest.TestCase):
    def test_add_turn_accumulates_word_counts(self) -> None:
        usage = UsageSummary()
        after = usage.add_turn('hello world', 'greetings back')
        self.assertEqual(after.input_tokens, 2)
        self.assertEqual(after.output_tokens, 2)
        self.assertEqual(usage.input_tokens, 0)  # original unchanged

    def test_add_turn_stacks_across_multiple_calls(self) -> None:
        usage = UsageSummary()
        usage = usage.add_turn('one', 'two')
        usage = usage.add_turn('three four', 'five')
        self.assertEqual(usage.input_tokens, 3)
        self.assertEqual(usage.output_tokens, 2)


class TestTranscriptStore(unittest.TestCase):
    def test_append_and_replay(self) -> None:
        store = TranscriptStore()
        store.append('msg1')
        store.append('msg2')
        self.assertEqual(store.replay(), ('msg1', 'msg2'))
        self.assertFalse(store.flushed)

    def test_flush_sets_flag(self) -> None:
        store = TranscriptStore()
        store.append('msg')
        store.flush()
        self.assertTrue(store.flushed)

    def test_compact_truncates_when_over_limit(self) -> None:
        store = TranscriptStore()
        for i in range(15):
            store.append(f'msg{i}')
        store.compact(keep_last=5)
        self.assertEqual(len(store.entries), 5)
        self.assertEqual(store.entries[0], 'msg10')
        self.assertEqual(store.replay()[-1], 'msg14')

    def test_compact_is_noop_when_under_limit(self) -> None:
        store = TranscriptStore()
        store.append('a')
        store.append('b')
        store.compact(keep_last=10)
        self.assertEqual(len(store.entries), 2)


class TestToolPermissionContext(unittest.TestCase):
    def test_blocks_by_exact_name(self) -> None:
        ctx = ToolPermissionContext.from_iterables(deny_names=['BashTool'])
        self.assertTrue(ctx.blocks('BashTool'))
        self.assertFalse(ctx.blocks('FileReadTool'))

    def test_blocks_by_prefix(self) -> None:
        ctx = ToolPermissionContext.from_iterables(deny_prefixes=['mcp'])
        self.assertTrue(ctx.blocks('MCPTool'))
        self.assertTrue(ctx.blocks('mcp_server'))
        self.assertFalse(ctx.blocks('BashTool'))

    def test_blocks_is_case_insensitive(self) -> None:
        ctx = ToolPermissionContext.from_iterables(deny_names=['bashTool'], deny_prefixes=['MCP'])
        self.assertTrue(ctx.blocks('BashTool'))
        self.assertTrue(ctx.blocks('mcptool'))

    def test_empty_context_blocks_nothing(self) -> None:
        ctx = ToolPermissionContext()
        self.assertFalse(ctx.blocks('anything'))


class TestFilterToolsByPermissionContext(unittest.TestCase):
    def test_returns_all_when_no_context(self) -> None:
        tools = (PortingModule('A', 'desc', 'src'), PortingModule('B', 'desc', 'src'))
        result = filter_tools_by_permission_context(tools, None)
        self.assertEqual(len(result), 2)

    def test_filters_blocked_tools(self) -> None:
        tools = (
            PortingModule('BashTool', 'desc', 'src'),
            PortingModule('FileReadTool', 'desc', 'src'),
        )
        ctx = ToolPermissionContext.from_iterables(deny_names=['BashTool'])
        result = filter_tools_by_permission_context(tools, ctx)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0].name, 'FileReadTool')


class TestQueryEnginePortSubmitMessage(unittest.TestCase):
    def _engine(self, **kwargs: object) -> QueryEnginePort:
        from src.port_manifest import build_port_manifest
        config = QueryEngineConfig(**kwargs)
        return QueryEnginePort(manifest=build_port_manifest(), config=config)

    def test_submit_message_returns_completed(self) -> None:
        engine = self._engine()
        result = engine.submit_message('hello')
        self.assertEqual(result.stop_reason, 'completed')
        self.assertTrue(result.output)

    def test_submit_message_tracks_usage(self) -> None:
        engine = self._engine()
        result = engine.submit_message('hello')
        self.assertGreater(result.usage.input_tokens, 0)

    def test_max_turns_reached(self) -> None:
        engine = self._engine(max_turns=1)
        engine.submit_message('first')
        result = engine.submit_message('second')
        self.assertEqual(result.stop_reason, 'max_turns_reached')
        self.assertIn('Max turns', result.output)

    def test_structured_output(self) -> None:
        engine = self._engine(structured_output=True)
        result = engine.submit_message('hello')
        parsed = json.loads(result.output)
        self.assertIn('summary', parsed)
        self.assertIn('session_id', parsed)

    def test_permission_denials_tracked(self) -> None:
        engine = self._engine()
        denial = PermissionDenial('BashTool', 'destructive')
        result = engine.submit_message('hello', denied_tools=(denial,))
        self.assertEqual(len(result.permission_denials), 1)


class TestQueryEnginePortCompact(unittest.TestCase):
    def test_compact_truncates_messages(self) -> None:
        from src.port_manifest import build_port_manifest
        config = QueryEngineConfig(compact_after_turns=3)
        engine = QueryEnginePort(manifest=build_port_manifest(), config=config)
        for i in range(5):
            engine.submit_message(f'msg{i}')
        self.assertLessEqual(len(engine.mutable_messages), 3)


class TestQueryEnginePortStream(unittest.TestCase):
    def test_stream_yields_events(self) -> None:
        from src.port_manifest import build_port_manifest
        engine = QueryEnginePort(manifest=build_port_manifest())
        events = list(engine.stream_submit_message('hello'))
        self.assertGreater(len(events), 0)
        self.assertEqual(events[0]['type'], 'message_start')
        self.assertEqual(events[-1]['type'], 'message_stop')


class TestSessionStoreRoundTrip(unittest.TestCase):
    def test_save_and_load_session(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            session = StoredSession(
                session_id='test-session',
                messages=('hello', 'world'),
                input_tokens=5,
                output_tokens=3,
            )
            path = save_session(session, directory=tmp_dir)
            self.assertTrue(path.exists())

            loaded = load_session('test-session', directory=tmp_dir)
            self.assertEqual(loaded.session_id, 'test-session')
            self.assertEqual(loaded.messages, ('hello', 'world'))
            self.assertEqual(loaded.input_tokens, 5)
            self.assertEqual(loaded.output_tokens, 3)


class TestCostTracker(unittest.TestCase):
    def test_record_accumulates(self) -> None:
        tracker = CostTracker()
        tracker.record('api', 100)
        tracker.record('cache', 50)
        self.assertEqual(tracker.total_units, 150)
        self.assertEqual(len(tracker.events), 2)

    def test_apply_cost_hook(self) -> None:
        tracker = CostTracker()
        result = apply_cost_hook(tracker, 'test', 42)
        self.assertEqual(result.total_units, 42)
        self.assertIs(result, tracker)


class TestDeferredInit(unittest.TestCase):
    def test_trusted_mode_enables_all(self) -> None:
        result = run_deferred_init(trusted=True)
        self.assertTrue(result.plugin_init)
        self.assertTrue(result.skill_init)
        self.assertTrue(result.mcp_prefetch)
        self.assertTrue(result.session_hooks)

    def test_untrusted_mode_disables_all(self) -> None:
        result = run_deferred_init(trusted=False)
        self.assertFalse(result.plugin_init)
        self.assertFalse(result.skill_init)
        self.assertFalse(result.mcp_prefetch)
        self.assertFalse(result.session_hooks)

    def test_as_lines_returns_strings(self) -> None:
        result = run_deferred_init(trusted=True)
        lines = result.as_lines()
        self.assertEqual(len(lines), 4)
        self.assertTrue(all(isinstance(line, str) for line in lines))


class TestHistoryLog(unittest.TestCase):
    def test_add_and_as_markdown(self) -> None:
        log = HistoryLog()
        log.add('routing', 'routed 5 matches')
        log.add('execution', 'executed 2 tools')
        md = log.as_markdown()
        self.assertIn('routing', md)
        self.assertIn('routed 5 matches', md)
        self.assertIn('execution', md)


class TestCommandGraphFlattened(unittest.TestCase):
    def test_command_graph_flattened(self) -> None:
        from src.command_graph import build_command_graph
        graph = build_command_graph()
        flat = graph.flattened()
        self.assertEqual(len(flat), len(graph.builtins) + len(graph.plugin_like) + len(graph.skill_like))


class TestCommandLookups(unittest.TestCase):
    def test_get_command_found(self) -> None:
        # Use the first command from the snapshot
        name = PORTED_COMMANDS[0].name
        result = get_command(name)
        self.assertIsNotNone(result)
        self.assertEqual(result.name, name)

    def test_get_command_not_found(self) -> None:
        self.assertIsNone(get_command('nonexistent_command_xyz'))

    def test_execute_command_found(self) -> None:
        name = PORTED_COMMANDS[0].name
        result = execute_command(name, 'test prompt')
        self.assertTrue(result.handled)
        self.assertIn(name, result.message)

    def test_execute_command_not_found(self) -> None:
        result = execute_command('nonexistent_command_xyz', 'test prompt')
        self.assertFalse(result.handled)

    def test_find_commands_filter(self) -> None:
        results = find_commands('review', limit=5)
        self.assertLessEqual(len(results), 5)

    def test_command_names_returns_list(self) -> None:
        names = command_names()
        self.assertGreater(len(names), 0)

    def test_built_in_command_names_returns_frozenset(self) -> None:
        names = built_in_command_names()
        self.assertIsInstance(names, frozenset)
        self.assertGreater(len(names), 0)

    def test_get_commands_excludes_plugins(self) -> None:
        all_commands = get_commands()
        no_plugins = get_commands(include_plugin_commands=False)
        self.assertLessEqual(len(no_plugins), len(all_commands))

    def test_get_commands_excludes_skills(self) -> None:
        all_commands = get_commands()
        no_skills = get_commands(include_skill_commands=False)
        self.assertLessEqual(len(no_skills), len(all_commands))

    def test_render_command_index_with_query(self) -> None:
        output = render_command_index(query='review', limit=5)
        self.assertIn('Filtered by: review', output)


class TestToolLookups(unittest.TestCase):
    def test_get_tool_found(self) -> None:
        name = PORTED_TOOLS[0].name
        result = get_tool(name)
        self.assertIsNotNone(result)

    def test_get_tool_not_found(self) -> None:
        self.assertIsNone(get_tool('nonexistent_tool_xyz'))

    def test_execute_tool_found(self) -> None:
        name = PORTED_TOOLS[0].name
        result = execute_tool(name, 'payload')
        self.assertTrue(result.handled)

    def test_execute_tool_not_found(self) -> None:
        result = execute_tool('nonexistent_tool_xyz', 'payload')
        self.assertFalse(result.handled)

    def test_find_tools_filter(self) -> None:
        results = find_tools('MCP', limit=5)
        self.assertLessEqual(len(results), 5)

    def test_tool_names_returns_list(self) -> None:
        names = tool_names()
        self.assertGreater(len(names), 0)

    def test_get_tools_simple_mode(self) -> None:
        tools = get_tools(simple_mode=True)
        names = [t.name for t in tools]
        self.assertIn('BashTool', names)

    def test_get_tools_exclude_mcp(self) -> None:
        all_tools = get_tools()
        no_mcp = get_tools(include_mcp=False)
        self.assertLessEqual(len(no_mcp), len(all_tools))

    def test_render_tool_index_with_query(self) -> None:
        output = render_tool_index(query='MCP', limit=5)
        self.assertIn('Filtered by: MCP', output)


class TestPortRuntimeScoring(unittest.TestCase):
    def test_score_matches_name(self) -> None:
        from src.runtime import PortRuntime
        module = PortingModule('review', 'review command', 'commands')
        score = PortRuntime._score({'review'}, module)
        self.assertGreater(score, 0)

    def test_score_zero_on_no_match(self) -> None:
        from src.runtime import PortRuntime
        module = PortingModule('review', 'review command', 'commands')
        score = PortRuntime._score({'completely_different'}, module)
        self.assertEqual(score, 0)

    def test_infer_permission_denials_for_bash_tools(self) -> None:
        from src.runtime import PortRuntime, RoutedMatch
        runtime = PortRuntime()
        matches = [RoutedMatch('tool', 'BashTool', 'src', 5)]
        denials = runtime._infer_permission_denials(matches)
        self.assertEqual(len(denials), 1)
        self.assertEqual(denials[0].tool_name, 'BashTool')

    def test_infer_permission_denials_no_bash(self) -> None:
        from src.runtime import PortRuntime, RoutedMatch
        runtime = PortRuntime()
        matches = [RoutedMatch('tool', 'FileReadTool', 'src', 5)]
        denials = runtime._infer_permission_denials(matches)
        self.assertEqual(len(denials), 0)


class TestPortRuntimeRouteAndTurn(unittest.TestCase):
    def test_route_prompt_returns_matches(self) -> None:
        from src.runtime import PortRuntime
        runtime = PortRuntime()
        matches = runtime.route_prompt('review MCP tool', limit=5)
        self.assertIsInstance(matches, list)

    def test_turn_loop_respects_max_turns(self) -> None:
        from src.runtime import PortRuntime
        runtime = PortRuntime()
        results = runtime.run_turn_loop('review MCP tool', limit=5, max_turns=1)
        self.assertEqual(len(results), 1)

    def test_turn_loop_stops_on_non_completed(self) -> None:
        from src.runtime import PortRuntime
        runtime = PortRuntime()
        # max_turns=3 but config budget will limit
        results = runtime.run_turn_loop('review MCP tool', limit=5, max_turns=3)
        self.assertLessEqual(len(results), 3)


class TestBuildPortContext(unittest.TestCase):
    def test_context_counts_python_files(self) -> None:
        ctx = build_port_context()
        self.assertGreater(ctx.python_file_count, 0)

    def test_render_context_output(self) -> None:
        ctx = build_port_context()
        output = render_context(ctx)
        self.assertIn('Source root', output)
        self.assertIn('Python files', output)


class TestParityAuditResult(unittest.TestCase):
    def test_to_markdown_without_archive(self) -> None:
        result = ParityAuditResult(
            archive_present=False,
            root_file_coverage=(0, 18),
            directory_coverage=(0, 29),
            collapsed_dir_coverage=(0, 6),
            total_file_ratio=(0, 1902),
            command_entry_ratio=(0, 207),
            tool_entry_ratio=(0, 184),
            missing_root_targets=(),
            missing_directory_targets=(),
        )
        md = result.to_markdown()
        self.assertIn('unavailable', md)

    def test_to_markdown_with_archive(self) -> None:
        result = ParityAuditResult(
            archive_present=True,
            root_file_coverage=(18, 18),
            directory_coverage=(29, 29),
            collapsed_dir_coverage=(6, 6),
            total_file_ratio=(70, 1902),
            command_entry_ratio=(207, 207),
            tool_entry_ratio=(184, 184),
            missing_root_targets=(),
            missing_directory_targets=(),
        )
        md = result.to_markdown()
        self.assertIn('Root file coverage', md)
        self.assertIn('Collapsed directory coverage', md)
        self.assertIn('none', md)  # no missing items


class TestQueryEngineFromSavedSession(unittest.TestCase):
    def test_from_saved_session_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            # Save a session to the temp directory
            session = StoredSession(
                session_id='saved-test',
                messages=('hello',),
                input_tokens=1,
                output_tokens=1,
            )
            save_session(session, directory=tmp_dir)

            # Load it back and build an engine from the saved session
            stored = load_session('saved-test', directory=tmp_dir)
            from src.port_manifest import build_port_manifest
            from src.transcript import TranscriptStore
            transcript = TranscriptStore(entries=list(stored.messages), flushed=True)
            engine = QueryEnginePort(
                manifest=build_port_manifest(),
                session_id=stored.session_id,
                mutable_messages=list(stored.messages),
                total_usage=UsageSummary(stored.input_tokens, stored.output_tokens),
                transcript_store=transcript,
            )
            self.assertEqual(engine.session_id, 'saved-test')
            self.assertEqual(engine.total_usage.input_tokens, 1)


class TestPortingBacklog(unittest.TestCase):
    def test_summary_lines_format(self) -> None:
        backlog = PortingBacklog(
            title='Test',
            modules=[
                PortingModule('mod1', 'does things', 'src/a', 'mirrored'),
                PortingModule('mod2', 'does more', 'src/b', 'planned'),
            ],
        )
        lines = backlog.summary_lines()
        self.assertEqual(len(lines), 2)
        self.assertIn('[mirrored]', lines[0])
        self.assertIn('[planned]', lines[1])


class TestPortManifest(unittest.TestCase):
    def test_to_markdown_includes_total_files(self) -> None:
        from src.port_manifest import build_port_manifest
        manifest = build_port_manifest()
        md = manifest.to_markdown()
        self.assertIn('Total Python files', md)


if __name__ == '__main__':
    unittest.main()