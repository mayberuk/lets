package x

// foo counts requests.
var foo int

func Inc() { foo++ }

// leader is the node every Transition hands off to.
var leader string

func Transition(to string) { leader = to }
