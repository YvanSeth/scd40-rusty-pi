# Secret Stuff!

To set your wifi network and password put them into the relevant files but
without a trailing newling. I.e.

```
echo -n "my-wifi-network" > src/secrets/wifi-network
echo -n "some password" > src/secrets/wifi-password
```

This command can then be used to ensure changes don't get checked in:

```
git update-index --assume-unchanged src/secrets/wifi-network
git update-index --assume-unchanged src/secrets/wifi-password
```

