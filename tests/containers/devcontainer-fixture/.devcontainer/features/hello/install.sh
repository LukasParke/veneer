#!/bin/sh
set -e
mkdir -p /opt/hello/bin
printf '#!/bin/sh\necho "%s (loud=%s, user=%s)"\n' "$GREETING" "$LOUD" "$_REMOTE_USER" > /opt/hello/bin/hello
chmod +x /opt/hello/bin/hello
