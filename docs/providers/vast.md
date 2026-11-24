# Vast.ai

UsageDeck reads prepaid credit from Vast.ai's current-user endpoint. When the key may also list
instances, it calculates the current hourly burn, number of running or starting instances, and an
estimated runway (`credit / hourly burn`). Credit remains available when the key is billing-only;
the optional fleet metrics are simply hidden.

Add a read-only key from **Customize → Vast.ai → API Key**. The key is stored in the operating
system credential store. You can alternatively set `VAST_API_KEY`, use
`~/.config/vastai/vast_api_key`, or use `~/.vast_api_key`.

The provider performs only authenticated `GET` requests. It never starts, stops, or destroys an
instance.
