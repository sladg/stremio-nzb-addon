import addon from './addon';

const port = process.env.PORT || 1337;

addon.listen(port, () => {
  console.log(`Addon running on port ${port}`);
});
