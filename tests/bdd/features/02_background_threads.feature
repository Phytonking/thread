@offline
Feature: Background threads
  As a developer
  I want main to start named background threads that work in parallel and report back
  So that several pieces of work progress while I keep talking to main

  Background:
    Given a git repository
    And the model server is running
    And I open thread

  Scenario: Main starts a named background thread in its own worktree
    When I type "spawn thread Fix Login: fix the login redirect, worktree"
    Then I see "◆ spawned fix-login"
    And branch "thread/fix-login" exists
    And worktree ".thread/worktrees/fix-login" exists

  Scenario: A finished background thread reports back and main tells me
    When I type "spawn thread probe: look around"
    Then I see "✓ [probe] finished"
    And I see "echo: [from probe] finished"

  Scenario: Thread names stay unique
    Given I type "spawn thread probe: first"
    And I see "✓ [probe] finished"
    When I type "spawn thread probe: second"
    Then I see "◆ spawned probe-2"

  Scenario: Listing threads shows every open thread
    Given I type "spawn thread probe: look around"
    And I see "✓ [probe] finished"
    When I type "/threads"
    Then I see "probe"
    And I see "▸ main"

  Scenario Outline: Switching to a thread by part of its name
    Given I type "spawn thread search-indexer: rebuild the index"
    And I see "✓ [search-indexer] finished"
    When I type "/switch <query>"
    Then I see "── search-indexer"
    And I see "task: rebuild the index"

    Examples:
      | query          |
      | search-indexer |
      | search         |
      | indexer        |

  Scenario: Talking inside a background thread and coming back to main
    Given I type "spawn thread probe: look around"
    And I see "✓ [probe] finished"
    And I type "/switch probe"
    When I type "status please"
    Then I see "echo: status please"
    When I type "/switch main"
    Then I see "── main"

  Scenario: Switching to a thread that doesn't exist
    When I type "/switch nothing-here"
    Then I see "no thread matches"
