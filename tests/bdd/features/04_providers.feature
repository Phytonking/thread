@offline
Feature: Providers
  As a developer with subscriptions to several AI tools
  I want to see which providers are ready and switch between them per thread
  So that I can use my subscriptions first and an API only when needed

  Background:
    Given a git repository
    And the model server is running

  Scenario: Login status lists every provider
    When I run "thread login"
    Then the output contains "claude"
    And the output contains "codex"
    And the output contains "grok"
    And the output contains "api"
    And the output contains "OpenAI-compatible"

  Scenario: The provider menu marks the current provider
    Given I open thread
    When I type "/provider"
    Then I see "▸"
    And I see "api"

  Scenario: An unknown provider is rejected
    Given I open thread
    When I type "/provider gemini"
    Then I see "unknown provider 'gemini'"

  Scenario: A provider that isn't installed is rejected
    Given the provider "grok" is not installed
    And I open thread
    When I type "/provider grok"
    Then I see "not installed"
