# Metalcraft Agent

Metalcraft Agent is a Rust application leveraging the Metalcraft framework to create reactive agents with various personas and functionalities. This agent can run interactively or execute specific tasks based on provided commands.

## Features

- **Reactive Agent Creation**: Utilizes Metalcraft for creating agents with customizable behaviors.
- **Persona Management**: Define and manage different personas for specialized tasks.
- **Tool Interaction**: Interact with various tools with the option for auto-approval.
- **Async Execution**: Built on Tokio for efficient async operations.

## Project Structure

- **Cargo.toml**: Configuration and dependencies for the Rust project.
- **src/main.rs**: Entry point for the application, handling command-line arguments and execution flow.
- **src/lib.rs**: Core module declarations.
- **src/tools/**: Contains implementations for various tools used by the agent.
- **docs/**: Documentation and analysis for project features and upgrades.
- **skills/**: Descriptions of various skills and methodologies employed by the agent.
- **tests/**: Contains unit and integration tests for different modules.

## Usage

```bash
metalcraft-agent [--auto-approve] <persona> [task]
```

- **`<persona>`**: The persona to be used by the agent. 
- **`[task]`**: Specific task to be executed. If omitted, the agent enters interactive mode.
- **`--auto-approve`**: Automatically approve prompts for all tools.

Ensure you have the correct personas set up in the `personas/` directory to use this functionality effectively.

## Dependencies

Key dependencies include:
- **Metalcraft**: For agent functionalities.
- **Tokio**: For asynchronous runtime support.
- **Serde**: For serialization and deserialization tasks.
- **Rustyline**: For interactive command-line input.

## Building and Testing

To build the project:

```bash
cargo build
```

To run tests:

```bash
cargo test
```

## Contributing

Contributions are welcome! Please make sure to update tests as appropriate and follow the existing style conventions.
