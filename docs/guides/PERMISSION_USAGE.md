# Permission System Usage Guide

## Overview

ION's permission system controls what agents can do—reading files, running commands, writing data, and accessing networks. It uses rules to allow, deny, or ask for approval before actions execute.

---

## 1. What Permissions Do

The permission system intercepts all tool calls and applies rules before execution:

- **Allow**: Action executes automatically
- **Deny**: Action is blocked immediately
- **Ask**: Prompts user for approval via UI channel

This protects your system from accidental or malicious actions while giving you control over what agents can do.

---

## 2. Configure Rules in settings.json

### Global Configuration (applies to all projects)

```bash
mkdir -p ~/.ion
cat > ~/.ion/settings.json << 'EOF'
{
  "permissions": {
    "rules": [
      {"subject": "command.run", "pattern": "echo *", "decision": "allow", "scope": "project"},
      {"subject": "file.read", "pattern": "**/.env*", "decision": "deny", "scope": "project"}
    ]
  }
}
EOF
```

### Project Configuration (overrides global)

```bash
mkdir -p <project>/.ion
cat > <project>/.ion/settings.json << 'EOF'
{
  "permissions": {
    "rules": [
      {"subject": "command.run", "pattern": "npm *", "decision": "allow", "scope": "project"}
    ]
  }
}
EOF
```

### Rule Fields

| Field | Description |
|-------|-------------|
| `subject` | What's being controlled (command.run, file.read, file.write, file.delete, network.connect, *) |
| `pattern` | Match pattern (exact, wildcard *, or glob like **/*.env) |
| `decision` | allow, deny, or ask |
| `scope` | session (memory only) or project (persisted to settings.json) |

---

## 3. CLI Commands (extension_rpc for Permission)

### List All Rules

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"list_rules"}'
```

### Add a Session Rule (temporary, current session only)

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"gh *","decision":"allow","scope":"session"}}'
```

### Add a Project Rule (persisted to settings.json)

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.read","pattern":"**/secret/*","decision":"deny","scope":"project"}}'
```

---

## 4. Stored Decisions (Always Allow)

Decisions can be remembered at three levels:

1. **Session Only**: Rules added with `scope:session` disappear after restart
2. **Project Scope**: Rules added with `scope:project` write to `.ion/settings.json` and persist across restarts
3. **Global Scope**: Rules in `~/.ion/settings.json` apply to all projects

Example of allowing a command for the entire session:

```bash
# Add rule that allows all echo commands this session
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"echo *","decision":"allow","scope":"session"}}'

# Now echo runs without asking
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'
```

---

## 5. CommandGuard Modes

Permission modes control default behavior when no rule matches:

| Mode | Behavior | Use Case |
|------|----------|----------|
| `default` | Ask for approval on unmatched actions | Normal interactive development |
| `acceptEdits` | Auto-allow file edits, ask for commands | Quick iteration |
| `bypassPermissions` | Allow everything (DANGEROUS) | Trusted environments only |
| `dontAsk` | Deny unmatched actions (never ask) | CI/CD pipelines |
| `plan` | Ask on all modifications | Review phases |
| `readonly` | Deny all writes | Read-only auditing |

Set mode via CLI flag:

```bash
# Start with readonly mode
ion --permission-mode readonly

# Start with bypass (use with caution!)
ion --permission-mode bypassPermissions
```

---

## 6. Agent Tool Restrictions

Tools can be restricted by subject and pattern:

### Restrict File Operations

```json
{
  "permissions": {
    "rules": [
      {"subject": "file.read", "pattern": "**/.env*", "decision": "deny"},
      {"subject": "file.write", "pattern": "**/config/*", "decision": "ask"}
    ]
  }
}
```

### Restrict Commands

```json
{
  "permissions": {
    "rules": [
      {"subject": "command.run", "pattern": "rm -rf *", "decision": "deny"},
      {"subject": "command.run", "pattern": "npm *", "decision": "allow"}
    ]
  }
}
```

### Restrict All Operations

```json
{
  "permissions": {
    "rules": [
      {"subject": "*", "pattern": "*", "decision": "deny"}
    ]
  }
}
```

---

## Priority Rules

1. **Deny** always overrides Allow on the same subject+pattern
2. Session rules override project rules
3. Project rules override global rules
4. Deny rules have highest priority in decision pipeline

---

## Practical Examples

### Allow safe npm commands, block dangerous rm

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"npm *","decision":"allow","scope":"project"}}'

ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"rm -rf *","decision":"deny","scope":"project"}}'
```

### Deny reading sensitive files

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.read","pattern":"**/secret/*","decision":"deny","scope":"project"}}'

ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.read","pattern":"~/.ssh/*","decision":"deny","scope":"project"}}'
```

### Ask before editing config files

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"file.write","pattern":"**/config/**/*.json","decision":"ask","scope":"project"}}'
```

---

## Built-in Permission Profiles

ION includes pre-configured profiles:

| Profile | Effect |
|---------|--------|
| `:read-only` | All filesystem read-only, commands allowed, no network |
| `:workspace` | Write access to workspace root, `.git/` and `.codex/` read-only, minimal paths readable |
| `:danger-full-access` | No restrictions (equivalent to no sandbox) |

Use via CLI:

```bash
ion --permission-profile :read-only
```