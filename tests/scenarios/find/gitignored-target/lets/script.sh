mkdir -p src target/debug target/locked && printf 'fn quokka() {}\n' > src/lib.rs && printf 'quokka built\n' > target/debug/out.txt && printf 'quokka locked\n' > target/locked/deep.txt && printf 'target/\n' >> .gitignore && chmod 000 target/locked

lets find quokka .

lets find quokka . --no-ignore

lets find quokka . --hidden

chmod 755 target/locked
