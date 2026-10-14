#!/bin/sh
# Cheap stand-in for claude / grok / cursor-agent / codex / opencode / vendor CLIs.
# ATR_FAKE_SIGNED=1 or ATR_FAKE_SIGNED_<basename>=1 means signed in.
name=$(basename "$0")
key=$(echo "$name" | tr '-' '_')
eval "signed=\${ATR_FAKE_SIGNED_${key}:-${ATR_FAKE_SIGNED:-0}}"
eval "keychain=\${ATR_FAKE_KEYCHAIN_${key}:-${ATR_FAKE_KEYCHAIN:-0}}"
if [ "$keychain" = "1" ]; then
  if [ "$1" = "--version" ]; then
    printf '%s keychain is locked.\033[0m\n' "$name"
    exit 1
  fi
  echo "Unable to read from the keychain: User interaction is not allowed."
  exit 1
fi
if [ "$1" = "--version" ]; then
  echo "$name 1.2.3"
  exit 0
fi
if [ "$1" = "models" ]; then
  echo "moonshotai/kimi-k2.6"
  echo "ollama/llama3.2"
  exit 0
fi
if [ "$1" = "login" ]; then
  if [ "$signed" = "1" ]; then
    echo "logged in"
    exit 0
  fi
  echo "not logged in"
  exit 1
fi
if [ "$1" = "auth" ]; then
  if [ "$2" = "list" ]; then
    if [ "$signed" = "1" ]; then
      echo "moonshotai"
      echo "ollama"
      exit 0
    fi
    echo "no providers logged in"
    exit 0
  fi
  if [ "$signed" = "1" ]; then
    echo "logged in as testdev"
    exit 0
  fi
  echo "not logged in"
  exit 1
fi
exit 0
