@offline
Feature: Main thread
  As a developer working in a repository
  I want `thread` to drop me onto a persistent main conversation
  So that I can pick up where I left off without re-explaining context

  Background:
    Given a git repository
    And the model server is running

  Scenario: Opening thread in a repository lands on main
    When I open thread
    Then I see "── main"
    And I see "/help for commands"

  Scenario: Main answers a message
    Given I open thread
    When I type "hello there"
    Then I see "echo: hello there"

  Scenario: Main remembers the conversation across launches
    Given I open thread
    And I type "the codeword is pineapple"
    And I see "echo:"
    And I type "exit"
    And thread has exited
    When I open thread
    And I type "what was the codeword?"
    Then the model received "pineapple" in the conversation

  Scenario: Typing exit quits
    Given I open thread
    When I type "exit"
    Then thread exits successfully
