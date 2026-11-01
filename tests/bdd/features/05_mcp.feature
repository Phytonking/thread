@offline
Feature: MCP servers
  As a developer with my own tools
  I want to register MCP servers once
  So that every thread can use them

  Background:
    Given a git repository
    And the model server is running
    And I open thread

  Scenario: The built-in server is always listed
    When I type "/mcp"
    Then I see "thread"
    And I see "built in"

  Scenario: Adding and removing servers
    When I type "/mcp add docs https://example.com/mcp"
    Then I see "added docs"
    When I type "/mcp add gh gh-mcp --read-only"
    Then I see "added gh"
    When I type "/mcp"
    Then I see "https://example.com/mcp"
    And I see "gh-mcp --read-only"
    When I type "/mcp rm docs"
    Then I see "removed docs"
    And the MCP config has server "gh" but not "docs"

  Scenario: An invalid server name is rejected
    When I type "/mcp add thread something"
    Then I see "not 'thread'"
