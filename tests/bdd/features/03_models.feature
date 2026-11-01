@offline
Feature: Choosing models
  As a developer
  I want a live menu of the models my provider actually offers
  So that I never send a model name the provider can't run

  Background:
    Given a git repository
    And the model server offers models "gpt-large, gpt-small, gpt-mini"
    And I open thread

  Scenario: The model menu comes from the provider
    When I type "/model"
    Then I see "1. gpt-large"
    And I see "2. gpt-small"
    And I see "3. gpt-mini"

  Scenario Outline: Picking a model by number or by name
    When I type "/model <choice>"
    Then I see "main now uses <model>"
    When I type "hi"
    Then I see "echo: hi"
    And the last request to the model used model "<model>"

    Examples:
      | choice    | model     |
      | 2         | gpt-small |
      | gpt-mini  | gpt-mini  |
      | large     | gpt-large |

  Scenario: An ambiguous partial name asks me to choose
    When I type "/model gpt"
    Then I see "matches"
    And I see "pick one with /model <number>"

  Scenario: A model the provider doesn't offer is rejected
    When I type "/model luna"
    Then I see "doesn't offer 'luna'"

  Scenario: Resetting to the default model
    Given I type "/model 3"
    When I type "/model default"
    Then I see "default model"

  Scenario: A spawned thread inherits main's model
    Given I type "/model 2"
    When I type "spawn thread probe: look around"
    Then I see "✓ [probe] finished"
    And a request from thread "probe" used model "gpt-small"

  Scenario: Main can spawn a thread on a specific model
    Given I type "/model 2"
    When I type "spawn thread probe with model mini: look around"
    Then I see "◆ spawned probe (api gpt-mini)"
    And I see "✓ [probe] finished"
    And a request from thread "probe" used model "gpt-mini"

  Scenario: Spawning with a model the provider doesn't offer fails without creating a thread
    When I type "spawn thread probe with model luna: look around"
    Then I see "doesn't offer 'luna'"
    And I type "/threads"
    And I do not see "probe"

  Scenario: Main can move a thread to another model
    Given I type "spawn thread probe: look around"
    And I see "✓ [probe] finished"
    When I type "move probe to model large"
    Then the model received "probe now runs on api with gpt-large" in the conversation
    And I type "/switch probe"
    And I type "hi"
    And I see "echo: hi"
    And a request from thread "probe" used model "gpt-large"
