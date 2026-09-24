mkdir legacy && printf 'fn good() {}\nlet s = "\xff\xfe";\nfn target_fn() { 1 }\n' > legacy/m.rs && printf '\xff\xfet\0a\0r\0g\0e\0t\0_\0f\0n\0' > legacy/u16.txt && printf 'target_fn\0\n' > legacy/blob.bin

lets find target_fn legacy
