# Abliteration.ai

UsageDeck reads prepaid credit information from Abliteration.ai. Model API keys use
`GET /v1/credits`; organization Admin keys with billing-read permission use
`GET /v1/organization/balance`.

Add a billing-readable key from **Customize → Abliteration.ai → API Key**. The key is stored in
the operating system credential store. You can alternatively set `ABLITERATION_API_KEY` or place a
JSON object containing `apiKey`, `api_key`, or `key` at `~/.config/abliteration/key.json`.

For the least privilege, use an organization Admin key with billing-read permission. UsageDeck
selects the correct endpoint from the key type and never sends inference requests with the key.
