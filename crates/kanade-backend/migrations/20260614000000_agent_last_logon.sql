-- #655 prerequisite: per-agent "last signed-in account". The agent
-- reports, in every heartbeat, the account the Windows sign-in screen
-- last used (HKLM\...\Authentication\LogonUI) — both the DOMAIN\sam
-- login name and its display name. Stored here so the Agents list can
-- show who last used each host. NULL = not reported (never-signed-in
-- host, pre-#655 agent, or non-Windows agent).
ALTER TABLE agents ADD COLUMN last_logon_user TEXT;
ALTER TABLE agents ADD COLUMN last_logon_display_name TEXT;
