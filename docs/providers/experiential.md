# Experiential Labs

UsageDeck reads the organization's credit wallet and plan allowances from Experiential Labs'
OpenRouter-compatible `GET https://api.experientiallabs.ai/api/v1/credits` endpoint. Any `xpl_` key
in the organization can call it, so an inference key works and no provisioning key is required.

Shown metrics:

- **Balance**: `total_credits - total_usage`, in USD.
- **Daily Allowance** (paid plans): credits drawn from the plan's daily allowance, reset at
  00:00 UTC. 1 credit is 1¢, so it is shown in dollars.
- **Billing Cycle** (paid plans): monthly included credits used this cycle.
- **Free Tokens** (Free plan): the shared daily free-core token quota.
- **Credits Used**: cumulative wallet usage, in USD.

Add a key from **Customize → Experiential Labs → API Key**. The key is stored in the operating
system credential store. You can alternatively set `EXPERIENTIAL_API_KEY` (or
`EXPERIENTIAL_LABS_API_KEY`), or place a JSON object containing `apiKey`, `api_key`, or `key` at
`~/.config/experiential/key.json`.

The provider performs a single authenticated `GET` and never sends inference requests with the key.
