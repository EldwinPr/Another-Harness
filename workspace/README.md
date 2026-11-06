The directory the chat runs in. read_file and bash start here, so the model works in this folder
and not in src/. Everything in it except this file is ignored by git (see .gitignore).
It is a working directory, not a sandbox: absolute paths still reach the rest of the machine.
