@offline
Feature: Simple commands
  As a developer
  I want a small, predictable set of commands
  So that I don't have to learn a manual to use thread

  Background:
    Given a git repository
    And the model server is running
    And I open thread

  Scenario: Help lists exactly the essential commands
    When I type "/help"
    Then I see "/model"
    And I see "/provider"
    And I see "/mcp"
    And I see "/threads"
    And I see "/switch"
    And I see "exit"
    And I do not see "/trust"
    And I do not see "/new"

  Scenario: Unknown commands point to help
    When I type "/frobnicate"
    Then I see "unknown command /frobnicate"

  Scenario: Commands work without the slash
    Given I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    When I type "switch probe"
    Then I see "── probe"
    And I type "main"
    And I see "── main"

  Scenario: A sentence that starts like a command is still a message
    When I type "switch to the other approach please"
    Then the model received "switch to the other approach please" in the conversation

  Scenario: /thread is another name for /switch
    Given I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    When I type "/thread probe"
    Then I see "── probe"

  Scenario: /switch with no name offers a list to pick from
    Given I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    When I type "/switch"
    Then I see "switch to"
    And I type "2"
    And I see "── probe"

  Scenario: /rm deletes a thread so it no longer shows up
    Given I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    When I type "/rm probe"
    Then I see "removed probe"
    And I type "/threads"
    And I do not see "probe"

  Scenario: Thinking shows as one line and expands on request
    When I type "think about pricing"
    Then I see "∴ Weighing pricing (+2 lines · /expand)"
    And I see "thought it through"
    And I type "/expand"
    And I see "second consideration"
    And I type "/expand all"
    And I type "think about risks"
    And I see "second consideration"
    And I type "/collapse"
    And I type "think about scope"
    And I see "∴ Weighing scope (+2 lines"

  Scenario: ! runs a shell command and the next message carries its output
    When I type "! echo hello-from-shell"
    Then I see "hello-from-shell"
    And I type "what did that print"
    And the model received "[I ran `echo hello-from-shell`" in the conversation

  Scenario: Ctrl-C in a thread goes back to main
    Given I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    And I type "/switch probe"
    And I see "── probe"
    When I press ctrl-c
    Then I see "── main"
    And I type "hi"
    And I see "echo: hi"

  Scenario: Ctrl-C on main hands off and quits like exit
    Given I type "hi"
    And I see "echo: hi"
    When I press ctrl-c
    Then I see "handing off"
    And thread exits successfully

  Scenario: /clear clears without reprinting anything
    Given I type "hi"
    And I see "echo: hi"
    When I type "clear"
    Then I do not see "── main"
    And I do not see "echo: hi"
