#!/bin/bash
set -e

# Execute the command that is passed to the docker run command.
# If no command is passed, it will execute the CMD from the Dockerfile.
exec "$@"
