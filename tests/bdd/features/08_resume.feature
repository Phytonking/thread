@offline
Feature: Resuming work
  As a developer
  I want threads that are still working when I quit to carry on next time
  So that quitting never loses work

  Background:
    Given a git repository
    And the model server is running

  Scenario: A running thread resumes on the next launch
    Given I open thread with "--yolo"
    And I type "spawn thread slowpoke: slow task"
    And I see "◆ spawned slowpoke"
    When I type "exit"
    And I see "still running"
    And I type "l"
    And thread has exited
    And I open thread with "--yolo"
    Then I see "resumed: slowpoke"
    And I see "✓ [slowpoke] finished"

  Scenario: --resume offers every thread to pick from
    Given I open thread
    And I type "spawn thread probe: look around"
    And I see "◆ spawned probe"
    And I close thread
    When I open thread with "--resume"
    Then I see "resume a thread"
    And I see "probe"
    And I type "2"
    And I see "── probe"
