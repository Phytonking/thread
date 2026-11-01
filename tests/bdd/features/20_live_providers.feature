@live
Feature: Real subscriptions
  As a developer with Claude and ChatGPT subscriptions
  I want threads to run on my subscriptions with all their tools
  So that I get the same capabilities as using those tools directly

  Background:
    Given a git repository with a buggy calc.py

  @claude
  Scenario: Claude main delegates a fix to a background thread that commits it
    Given I open thread with "--provider claude --model haiku --yolo"
    When I type "Spawn one background thread named fix-add (worktree true) to fix the bug in calc.py add() and commit it. Keep it short."
    Then within 300 seconds I see "✓ [fix-add] finished"
    And branch "thread/fix-add" has a commit touching "calc.py"

  @codex
  Scenario: Codex main delegates through thread tools without yolo
    Given I open thread with "--provider codex"
    When I type "Start one background thread named notes-writer (no worktree) whose task is to create notes.md with one line: hello. Report back."
    Then within 300 seconds I see "✓ [notes-writer] finished"
    And thread "notes-writer" exists
    And file "notes.md" exists
    And I do not see "approval-blocked"

  @claude
  Scenario: Claude threads can call the harness tools
    Given I open thread with "--provider claude --model haiku --yolo"
    When I type "Call the thread list_threads tool, then reply with exactly: TOOLS-OK"
    Then within 180 seconds I see "TOOLS-OK"
    And thread "main" called a tool matching "list_threads"
