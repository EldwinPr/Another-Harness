The directory the chat runs in. bash starts here and the file tools (read_file, create_file,
write_file, delete_file) are confined to it, so the model works in this folder and not in src/. Everything in it except this file is ignored by git (see .gitignore).
The file tools refuse absolute paths, `..` and links that lead out; bash is not a sandbox:
absolute paths still reach the rest of the machine.
