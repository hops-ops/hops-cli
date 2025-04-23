# Context for AI Assistance

## Who I Am
A senior engineer with a focus on building software that’s simple, maintainable, and human-readable. I’ve seen enough convoluted systems to know that complexity often masks mediocrity. As E.F. Schumacher (or perhaps Einstein) said: *"Any intelligent fool can make things bigger, more complex, and more violent. It takes a touch of genius — and a lot of courage — to move in the opposite direction."* I live by this.

## My Preferences
- **Simplicity**: Favor straightforward solutions over clever ones. If it’s hard to explain to a junior engineer, it’s probably too complex.
- **Readability**: Code and documentation should be self-explanatory. Prioritize clear variable names, minimal abstraction layers, and plain English.
- **Practicality**: Solve the problem at hand, not hypothetical future ones. Avoid over-engineering.
- **Rust**: My language of choice for its balance of performance and safety. Keep idioms Rusty but accessible.

## Project Context
This is `hops-cli`, a Rust CLI tool for Harmony Operations.

## How to Help Me
- **Keep It Simple**: Propose minimal changes to fix issues. Avoid adding layers unless absolutely necessary.
- **Explain Clearly**: Use plain language, like you’re teaching a curious colleague. No jargon for jargon’s sake.
- **Debugging**: Suggest targeted logging or checks to isolate problems. I’ll run them and share output.
- **Code Style**: Match the existing style—flat structure, explicit error handling, and concise functions.
- **Solutions**: If the API’s the bottleneck, suggest practical alternatives (e.g., pagination, different endpoints) without turning this into a monolith.

## Example Prompt Response
**Prompt**: "How do I get more events?"
**Good Response**: "The current API endpoint limits us to 100 events per page. Add a loop to check the `Link` header in the response and fetch additional pages if they exist. Here’s a simple change to `main.rs` that logs the header—run it and share the output, then we can add pagination if needed."

## How to respond when giving me code
1. First tell me all of the files you are going to give me and their directory structure, ask to proceed (I'll say next or something like that). If it's one file, you can just give me the one file instead.
2. If there are more than one, send me one file at a time, ask to proceed until they are all sent. This gives me a chance to review each file and ask for changes. These changes may affect other files, and as such may require regenerating some. Keep track of these and adjust accordingly so I have all the correct files.

