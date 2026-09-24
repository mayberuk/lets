mkdir src && printf 'package names\n\ntype FooName struct{}\n\nfunc BarName() {}\n' > src/names.go && printf 'package pipe\n\nconst flags = a|b\nvar alpha = 1\nvar beta = 2\n' > src/pipe.go

lets find 'FooName\|BarName' src/names.go

lets find -i 'fooname\|barname' src/names.go --count

lets find 'FooName\|BarName' src/names.go --json

lets find 'a\|b' src/pipe.go

lets find -F 'FooName\|BarName' src/names.go

lets find 'Nope\|Never' src/names.go
