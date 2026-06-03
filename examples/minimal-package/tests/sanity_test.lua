local t = fkst.test

return {
  test_sanity = function()
    t.eq(1 + 1, 2)
  end,
  test_raises = function()
    t.raises(function()
      error("expected failure")
    end)
  end,
  test_nil = function()
    t.is_nil(nil)
  end,
}
