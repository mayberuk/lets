mkdir -p vault/sealed && printf 'needle open\n' > vault/open.txt && printf 'needle sealed\n' > vault/sealed/secret.txt && chmod 000 vault/sealed

lets find needle vault

chmod 755 vault/sealed
