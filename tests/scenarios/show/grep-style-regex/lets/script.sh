mkdir src && printf 'package names\n\ntype FooName struct{}\n\nfunc BarName() {}\n' > src/names.go && printf 'package pipe\n\nconst flags = a|b\nvar alpha = 1\nvar beta = 2\n' > src/pipe.go

lets show "src/names.go@'FooName\|Nope'"

lets show "src/pipe.go@'a\|b'"

lets show "src/names.go@'Nope\|Never'"
