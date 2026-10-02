# Codexctl

Codexctl manages several OpenAI Codex logins for one person. It can keep them on
each machine or on a private account server that all of that person's machines share.

## Language

### People and machines

**Company user**:
A person identified by company SSO on the account server.
_Avoid_: account owner, user (alone)

**Machine**:
A computer that runs codexctl and is connected to the account server.
_Avoid_: device (except in the `codexctl devices` command and the OpenAI device code)

**Account server**:
The private service that keeps OpenAI refresh credentials and supplies access tokens to machines.
_Avoid_: central server, broker

### Accounts

**Alias**:
The name a company user gives to a profile or a server account.
_Avoid_: account name

**Profile**:
An OpenAI login saved on one machine under an alias.
_Avoid_: local account

**Server account**:
An OpenAI login kept on the account server under one company user and one alias.
Every machine of that company user can use it at the same time.
_Avoid_: remote account, transferred alias, shared account

**Refresh owner**:
The single process that may refresh a server account's OpenAI credentials.
A server account has at most one refresh owner at any time.
_Avoid_: owner (alone), credential owner, login owner

### Operations

**Migration**:
Moving profiles from a machine to the account server so that they become server accounts.
_Avoid_: transfer, handoff, import

**Login renewal**:
A new OpenAI sign-in for an existing server account that gives the account server fresh credentials.
_Avoid_: relogin, re-login
