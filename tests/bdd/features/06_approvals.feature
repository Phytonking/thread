@offline
Feature: Approvals
  As a developer
  I want to approve commands before an agent runs them
  So that nothing changes on my machine without my say-so

  Background:
    Given a git repository
    And the model server is running
    And I open thread

  Scenario: Approving a command lets it run
    When I type "run: touch approved.txt"
    Then I see "allow bash"
    When I type "y"
    Then I see "ran:"
    And file "approved.txt" exists

  Scenario: Denying a command stops it
    When I type "run: touch denied.txt"
    Then I see "allow bash"
    When I type "n"
    Then I see "denied"
    And file "denied.txt" does not exist

  Scenario Outline: Read-only commands run without asking
    When I type "run: <command>"
    Then I see "ran:"
    And I do not see "allow bash"

    Examples:
      | command        |
      | pwd            |
      | ls             |
      | git status     |

  Scenario: Commands that change things still ask, even when they start harmlessly
    When I type "run: ls && touch sneaky.txt"
    Then I see "allow bash"
    When I type "n"
    Then file "sneaky.txt" does not exist

  Scenario: Yolo mode runs commands without asking
    Given I close thread
    And I open thread with "--yolo"
    When I type "run: touch yolo.txt"
    Then I see "ran:"
    And file "yolo.txt" exists

  Scenario: Answering a background thread's request from main
    When I type "spawn thread worker: run: touch bg.txt"
    Then I see "⚠ [worker] wants to bash"
    And I see "/y worker"
    When I type "/y worker"
    Then I see "✓ allowed worker"
    And I see "✓ [worker] finished"
    And file "bg.txt" exists

  Scenario: Always allowing a tool stops the questions
    When I type "run: touch one.txt"
    Then I see "a = always allow bash"
    When I type "a"
    Then I see "bash won't ask again"
    When I type "run: touch two.txt"
    Then I see "ran:"
    And I do not see "allow bash"
    And file "two.txt" exists
