#!/bin/sh
# The executable stays read-only while parallel tests write their individual scripts.
exec /bin/sh "$0.body" "$@"
