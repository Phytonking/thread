@offline
Feature: Local models
  As a developer who wants to run threads on my own machine
  I want thread to drive Ollama, Apple's on-device model, or any local OpenAI-compatible server
  So that I can work offline and without an API key

  Background:
    Given a git repository

  Scenario: Status lists installed Ollama models and their tool support
    Given the model server is running as ollama with models "qwen3:8b, gemma3:4b-notools"
    When I run "thread login"
    Then the output contains "2 models, 1 with tool calling"

  Scenario: A thread runs on Ollama with no key, from a partial model name
    Given the model server is running as ollama with models "qwen3:8b, gemma3:4b-notools"
    When I run "thread ask 'hello there' --model qwen3"
    Then the command succeeds
    And the output contains "echo: hello there"
    And every request to the model used model "qwen3:8b"

  Scenario: Ollama defaults to an installed model that can call tools
    Given the model server is running as ollama with models "gemma3:4b-notools, qwen3:8b"
    When I run "thread ask 'run: echo from-ollama' --yolo"
    Then the command succeeds
    And the output contains "ran: from-ollama"
    And a request from thread "main" used model "qwen3:8b"

  Scenario: A model without tool calling fails with a clear explanation
    Given the model server is running as ollama with models "qwen3:8b, gemma3:4b-notools"
    When I run "thread ask 'hello there' --model notools"
    Then the output contains "can't call tools"

  Scenario: A local OpenAI-compatible server needs no key
    Given the model server is running as local with models "llama-3.2-3b"
    When I run "thread login"
    Then the output contains "OpenAI-compatible server at"
    When I run "thread ask 'hello there' --yolo"
    Then the command succeeds
    And the output contains "echo: hello there"
    And every request to the model used model "llama-3.2-3b"

  Scenario: Apple Foundation Models through a bridge at THREAD_FM_URL
    Given the model server is running as fm with models "system"
    When I run "thread ask 'run: echo on-device' --yolo"
    Then the command succeeds
    And the output contains "ran: on-device"
    And every request to the model used model "system"
