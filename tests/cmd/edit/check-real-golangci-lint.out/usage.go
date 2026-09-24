package usage

func Usage(id string) int {
	base := 10
	limit := base*2 + len(id)
	return limit
}
