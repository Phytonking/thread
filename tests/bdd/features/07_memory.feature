@offline
Feature: Memory
  As a developer
  I want threads to remember durable facts and share corrections
  So that knowledge builds up across sessions instead of being re-learned

  Background:
    Given a git repository
    And the model server is running

  Scenario: A remembered fact is stored as tentative
    Given I open thread
    When I type "remember billing database postgres"
    Then I see "stored"
    And fact "billing · database = postgres" is "tentative"

  Scenario: A fact confirmed by a second session becomes active
    Given I open thread
    And I type "remember billing database postgres"
    And I see "stored"
    And I close thread
    When I open thread
    And I type "remember billing database postgres"
    Then I see "promoted"
    And fact "billing · database = postgres" is "active"

  Scenario: Recall finds stored facts
    Given I open thread
    And I type "remember deploy target fly.io"
    And I see "stored"
    And I close thread
    When I run "thread recall deploy"
    Then the output contains "fly.io"

  Scenario: A changed fact notifies the other threads that used it
    Given I open thread with "--yolo"
    And I type "spawn thread alpha: remember shared-db host supabase"
    And I see "✓ [alpha] finished"
    When I type "remember shared-db host rds"
    Then I see "superseded"
    And thread "alpha" has a memory notice containing "changed from \"supabase\" to \"rds\""
