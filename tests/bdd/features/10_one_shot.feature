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
