package usage

func Usage(id string) int {
	base := 10
	limit := base + len(id)
	return limit
}
