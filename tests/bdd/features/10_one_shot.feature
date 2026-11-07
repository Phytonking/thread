@offline
Feature: One-shot runs
  As a developer scripting thread
  I want to send one message and get the result without a REPL
  So that I can use thread from scripts and CI

  Background:
    Given a git repository
    And the model server is running

  Scenario: Ask waits for spawned threads and exits
    When I run "thread ask 'spawn thread probe: look around' --yolo"
    Then the command succeeds
    And the output contains "✓ [probe] finished"
    And the output contains "echo: [from probe] finished"

  Scenario: Ask waits for a report that arrives after a slow handoff
    When I run "thread ask 'spawn thread latecomer: write a late report' --yolo" with a 60 second limit
    Then the command succeeds
    And the output contains "echo: [from latecomer] finished"
    And thread "main" has no live run

  Scenario: Ask finishes promptly when every handoff request fails
    When I run "thread ask 'spawn thread fragile: a broken handoff' --yolo" with a 30 second limit
    Then the command succeeds
    And the output contains "✗ [fragile] handoff failed"
    And the output contains "✓ [fragile] finished (partial)"
    And the output contains "echo: [from fragile] finished (partial)"
    And the model received "the handoff summary failed; the report follows" in the conversation
    And no thread has a live run

  Scenario: Ask finishes promptly when the handoff answers prose instead of JSON
    When I run "thread ask 'spawn thread mumbler: a garbled handoff' --yolo" with a 30 second limit
    Then the command succeeds
    And the output contains "✗ [mumbler] handoff failed"
    And the output contains "echo: [from mumbler] finished (partial)"
    And no thread has a live run
